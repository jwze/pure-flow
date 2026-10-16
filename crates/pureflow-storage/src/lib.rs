// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! SQLite history, independent of legacy application files.
//!
//! Observed intervals are `[unix_ms-duration_ms, unix_ms)`. Bytes cannot be
//! reconstructed within an interval: all bytes are attributed to the minute and
//! local calendar day containing `unix_ms-1`. Coverage alone is split at bucket
//! boundaries. This deliberately does not prorate traffic or invent observations.
//! Queries are half-open and minute-aligned. Once minute data has expired, both
//! query boundaries must be local midnights; the entire query uses daily data.
//! Local-day UTC boundaries are persisted at write time (changing the host time
//! zone later does not reinterpret historical buckets).
use chrono::{Days, Local, NaiveDate, TimeZone};
use pureflow_core::{Bytes, IntervalRecord};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const MAX_RANGE: u64 = 366 * DAY;
const MAX_POINTS: usize = 1500;
// A partially covered bucket needs two break markers and one visible point.
const MAX_BUCKETS: usize = MAX_POINTS / 3;
const VERSION: &str = "1";

#[derive(Debug, Clone, Deserialize)]
pub struct HistoryQuery {
    pub from_ms: u64,
    pub to_ms: u64,
    pub network: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct HistoryResult {
    pub total: Bytes,
    pub covered_ms: u64,
    pub processes: Vec<ProcessRow>,
    pub points: Vec<HistoryPoint>,
    pub networks: Vec<String>,
}
#[derive(Debug, Serialize)]
pub struct ProcessRow {
    pub name: String,
    pub bytes: Bytes,
}
#[derive(Debug, Serialize)]
pub struct HistoryPoint {
    pub unix_ms: u64,
    pub download_bps: f64,
    pub upload_bps: f64,
    /// A break marker, not an observation. Observed rates always have gap=false,
    /// even when coverage is partial or the bucket contains an explicit gap.
    pub gap: bool,
}

/// Ordered internal history input. Batches preserve each interval's original attribution.
#[derive(Debug, Clone)]
pub enum HistoryEvent {
    Record {
        unix_ms: u64,
        record: IntervalRecord,
    },
    Gap(u64),
}
impl HistoryEvent {
    pub fn unix_ms(&self) -> u64 {
        match self {
            Self::Record { unix_ms, .. } | Self::Gap(unix_ms) => *unix_ms,
        }
    }
    /// Validate an event before accepting it for realtime publication, without touching SQLite.
    pub fn validate_after(&self, previous: Option<u64>) -> R<()> {
        let end = self.unix_ms();
        stamp(end)?;
        let start = match self {
            Self::Record { record, .. } => {
                if record.duration_ms == 0 || record.duration_ms > MINUTE {
                    return Err("invalid interval duration".into());
                }
                end.checked_sub(record.duration_ms)
                    .ok_or("invalid interval duration")?
            }
            Self::Gap(_) => end,
        };
        stamp(start)?;
        if previous.is_some_and(|last| start < last) {
            return Err("history event overlaps previous accepted event".into());
        }
        Ok(())
    }
    /// Conservative retained-memory charge, including tree nodes and string capacities.
    pub fn retained_bytes(&self) -> usize {
        let base = std::mem::size_of::<Self>().saturating_add(2048);
        match self {
            Self::Gap(_) => base,
            Self::Record { record, .. } => base
                .saturating_add(record.network.as_ref().map_or(0, String::capacity))
                .saturating_add(record.processes.iter().fold(0usize, |sum, (name, _)| {
                    sum.saturating_add(name.capacity()).saturating_add(128)
                }))
                .saturating_add(record.warnings.capacity() * std::mem::size_of::<String>())
                .saturating_add(
                    record
                        .warnings
                        .iter()
                        .fold(0usize, |sum, s| sum.saturating_add(s.capacity())),
                ),
        }
    }
}

pub struct HistoryStore {
    conn: Connection,
    maintained_at: Instant,
}

fn cancelled(cancel: &AtomicBool) -> R<()> {
    if cancel.load(Ordering::Acquire) {
        Err("history query cancelled".into())
    } else {
        Ok(())
    }
}
type R<T> = Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn add(a: u64, b: u64) -> R<u64> {
    a.checked_add(b)
        .ok_or_else(|| "history sum exceeds u64".into())
}
fn parse(s: String) -> R<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid unsigned counter in history".into());
    }
    s.parse().map_err(err)
}
fn stamp(t: u64) -> R<i64> {
    let value = i64::try_from(t).map_err(err)?;
    Local
        .timestamp_millis_opt(value)
        .single()
        .ok_or("timestamp outside calendar range")?;
    Ok(value)
}
fn midnight(date: NaiveDate) -> R<u64> {
    // Some time zones move clocks at midnight. Use the earliest valid instant
    // of that local date; ambiguous midnights choose the earlier occurrence.
    for minute in 0..1440 {
        let naive = date
            .and_hms_opt(minute / 60, minute % 60, 0)
            .ok_or("invalid midnight")?;
        if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
            // The public timeline starts at the Unix epoch. Clip the first
            // local day instead of rejecting epoch-adjacent observations.
            return Ok(dt.timestamp_millis().max(0) as u64);
        }
    }
    Err("local date has no representable midnight".into())
}
fn day_bounds(t: u64) -> R<(u64, u64)> {
    let date = Local
        .timestamp_millis_opt(stamp(t)?)
        .single()
        .ok_or("invalid date")?
        .date_naive();
    Ok((
        midnight(date)?,
        midnight(date.succ_opt().ok_or("date overflow")?)?,
    ))
}
fn key(network: &Option<String>) -> String {
    network
        .as_ref()
        .map(|s| format!("s{s}"))
        .unwrap_or_else(|| "n".into())
}
fn ranges_merge(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = vec![];
    for (a, b) in ranges {
        if a >= b {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if a <= last.1 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        out.push((a, b));
    }
    out
}
fn covered(ranges: &[(u64, u64)], from: u64, to: u64) -> R<u64> {
    ranges.iter().try_fold(0, |sum, &(a, b)| {
        add(sum, b.min(to).saturating_sub(a.max(from)))
    })
}
fn meta(conn: &Connection, name: &str) -> R<Option<String>> {
    conn.query_row("SELECT value FROM metadata WHERE key=?1", [name], |r| {
        r.get(0)
    })
    .optional()
    .map_err(err)
}
fn put_meta(conn: &Connection, name: &str, value: u64) -> R<()> {
    conn.execute("INSERT INTO metadata(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![name,value.to_string()]).map_err(err)?;
    Ok(())
}

impl HistoryStore {
    pub fn open(path: &Path) -> R<Self> {
        let mut conn = Connection::open(path).map_err(err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(err)?;
        conn.execute_batch("PRAGMA foreign_keys=ON;").map_err(err)?;
        // Must be set before creating schema; existing databases are never vacuumed at startup.
        let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0)).map_err(err)?;
        if tables == 0 {
            conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")
                .map_err(err)?;
        }
        let tx = conn.transaction().map_err(err)?;
        let tables: i64 = tx.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0)).map_err(err)?;
        if tables == 0 {
            tx.execute_batch("CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                INSERT INTO metadata VALUES('schema_version','1');
                CREATE TABLE buckets(res INTEGER NOT NULL,start INTEGER NOT NULL,end INTEGER NOT NULL,network TEXT NOT NULL,received TEXT NOT NULL,sent TEXT NOT NULL,PRIMARY KEY(res,start,network));
                CREATE TABLE processes(res INTEGER NOT NULL,start INTEGER NOT NULL,network TEXT NOT NULL,name TEXT NOT NULL,received TEXT NOT NULL,sent TEXT NOT NULL,PRIMARY KEY(res,start,network,name),FOREIGN KEY(res,start,network) REFERENCES buckets(res,start,network) ON DELETE CASCADE);
                CREATE TABLE coverage(res INTEGER NOT NULL,start INTEGER NOT NULL,network TEXT NOT NULL,a INTEGER NOT NULL,b INTEGER NOT NULL,PRIMARY KEY(res,start,network,a),FOREIGN KEY(res,start,network) REFERENCES buckets(res,start,network) ON DELETE CASCADE);
                CREATE TABLE gaps(res INTEGER NOT NULL,start INTEGER NOT NULL,at INTEGER NOT NULL,PRIMARY KEY(res,start));").map_err(err)?;
        } else {
            let version = meta(&tx, "schema_version")?.ok_or("missing history schema version")?;
            // No earlier storage schema exists. Future versions must add explicit,
            // transactional migrations here, never guess or import Swift tables.
            if version != VERSION {
                return Err(format!("unsupported history schema version: {version}"));
            }
            // Validate the schema on reopen rather than accepting corrupt metadata.
            tx.prepare("SELECT b.res,b.start,b.end,b.network,b.received,b.sent,p.name,p.received,p.sent,c.a,c.b FROM buckets b LEFT JOIN processes p USING(res,start,network) LEFT JOIN coverage c USING(res,start,network) LIMIT 0").map_err(err)?;
            tx.prepare("SELECT res,start,at FROM gaps LIMIT 0")
                .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        Ok(Self {
            conn,
            maintained_at: Instant::now(),
        })
    }
    pub fn open_readonly(path: &Path) -> R<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(err)?;
        conn.busy_timeout(Duration::from_secs(2)).map_err(err)?;
        conn.execute_batch("PRAGMA query_only=ON;").map_err(err)?;
        if meta(&conn, "schema_version")?.as_deref() != Some(VERSION) {
            return Err("unsupported history schema".into());
        }
        Ok(Self {
            conn,
            maintained_at: Instant::now(),
        })
    }
    /// Durable event watermark, used to reject overlapping input before queue acceptance.
    pub fn persisted_time(&self) -> R<Option<u64>> {
        meta(&self.conn, "last_time")?.map(parse).transpose()
    }
    pub fn maintenance(&mut self) -> R<()> {
        if self.maintained_at.elapsed() < Duration::from_secs(3600) {
            return Ok(());
        }
        self.maintained_at = Instant::now();
        self.conn
            .execute_batch("PRAGMA incremental_vacuum(128);")
            .map_err(err)
    }
    pub fn compact(&mut self) -> R<()> {
        self.conn.flush_prepared_statement_cache();
        self.conn
            .execute_batch("PRAGMA auto_vacuum=INCREMENTAL; VACUUM;")
            .map_err(err)?;
        self.maintained_at = Instant::now();
        Ok(())
    }

    pub fn record(&mut self, unix_ms: u64, record: &IntervalRecord) -> R<()> {
        let tx = self.conn.transaction().map_err(err)?;
        apply_record(&tx, unix_ms, record)?;
        prune(&tx, unix_ms)?;
        tx.commit().map_err(err)
    }

    pub fn gap(&mut self, unix_ms: u64) -> R<()> {
        let tx = self.conn.transaction().map_err(err)?;
        apply_gap(&tx, unix_ms)?;
        prune(&tx, unix_ms)?;
        tx.commit().map_err(err)
    }

    /// One atomic transaction, with the same per-event accounting as synchronous writes.
    /// A failed event rolls back bytes, coverage, gaps and the durable time watermark.
    pub fn record_batch(&mut self, events: &[HistoryEvent]) -> R<()> {
        let Some(last) = events.last() else {
            return Ok(());
        };
        let tx = self.conn.transaction().map_err(err)?;
        for event in events {
            match event {
                HistoryEvent::Record { unix_ms, record } => apply_record(&tx, *unix_ms, record)?,
                HistoryEvent::Gap(unix_ms) => apply_gap(&tx, *unix_ms)?,
            }
        }
        prune(&tx, last.unix_ms())?;
        tx.commit().map_err(err)
    }

    pub fn networks(&self) -> R<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT network FROM buckets WHERE substr(network,1,1)='s' ORDER BY network").map_err(err)?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(err)?;
        rows.map(|v| v.map(|s| s[1..].to_owned()).map_err(err))
            .collect()
    }

    pub fn query(&self, query: HistoryQuery) -> R<HistoryResult> {
        self.query_with_cancel(query, &AtomicBool::new(false))
    }
    pub fn query_with_cancel(&self, query: HistoryQuery, cancel: &AtomicBool) -> R<HistoryResult> {
        cancelled(cancel)?;
        // A single read transaction keeps all range scans on the same database snapshot.
        let tx = self.conn.unchecked_transaction().map_err(err)?;
        let result = self.query_inner(query, cancel);
        tx.commit().map_err(err)?;
        result
    }
    fn query_inner(&self, query: HistoryQuery, cancel: &AtomicBool) -> R<HistoryResult> {
        let HistoryQuery {
            from_ms: from,
            to_ms: to,
            network,
        } = query;
        stamp(from)?;
        stamp(to)?;
        if to <= from || to - from > MAX_RANGE {
            return Err("query must be nonempty and at most 366 days".into());
        }
        if from % MINUTE != 0 || to % MINUTE != 0 {
            return Err("query boundaries must be minute-aligned".into());
        }
        let cutoff = meta(&self.conn, "minute_cutoff")?
            .map(parse)
            .transpose()?
            .unwrap_or(0);
        let res = if from < cutoff { 1 } else { 0 };
        if res == 1 && (day_bounds(from)?.0 != from || day_bounds(to)?.0 != to) {
            return Err("expired minute history requires local calendar day boundaries".into());
        }
        let step = if res == 1 {
            DAY
        } else if (to - from).div_ceil(MINUTE) <= MAX_BUCKETS as u64 {
            MINUTE
        } else if (to - from).div_ceil(HOUR) <= MAX_BUCKETS as u64 {
            HOUR
        } else {
            DAY
        };
        let mut trend: Vec<(u64, u64, Bytes)> = vec![];
        let mut cursor = from;
        while cursor < to {
            let end = if step == DAY {
                day_bounds(cursor)?.1.min(to)
            } else {
                add(cursor, step)?.min(to)
            };
            if end <= cursor {
                return Err("invalid calendar bucket".into());
            }
            trend.push((cursor, end, Bytes::default()));
            if trend.len() > MAX_BUCKETS {
                return Err("query exceeds bucket limit".into());
            }
            cursor = end;
        }
        let filter = network.as_ref().map(|s| format!("s{s}"));
        let mut total = Bytes::default();
        let mut process_totals = BTreeMap::<String, Bytes>::new();
        let mut all_coverage = vec![];
        let mut networks = BTreeSet::new();
        let mut stmt = self.conn.prepare_cached("SELECT start,network,received,sent FROM buckets WHERE res=?1 AND start>=?2 AND start<?3 AND (?4 IS NULL OR network=?4) ORDER BY start").map_err(err)?;
        let rows = stmt
            .query_map(params![res, from as i64, to as i64, filter], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(err)?;
        for row in rows {
            cancelled(cancel)?;
            let (a, net, received, sent) = row.map_err(err)?;
            let a = u64::try_from(a).map_err(err)?;
            if let Some(ssid) = net.strip_prefix('s') {
                networks.insert(ssid.to_owned());
            }
            let bytes = Bytes {
                received: parse(received)?,
                sent: parse(sent)?,
            };
            total = total.checked_add(bytes).map_err(err)?;
            let index = trend.partition_point(|point| point.1 <= a);
            if let Some(point) = trend.get_mut(index) {
                point.2 = point.2.checked_add(bytes).map_err(err)?;
            }
        }
        let mut ps = self.conn.prepare_cached("SELECT name,received,sent FROM processes WHERE res=?1 AND start>=?2 AND start<?3 AND (?4 IS NULL OR network=?4)").map_err(err)?;
        let rows = ps
            .query_map(params![res, from as i64, to as i64, filter], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(err)?;
        for row in rows {
            cancelled(cancel)?;
            let (name, r, s) = row.map_err(err)?;
            let current = process_totals.entry(name).or_default();
            *current = current
                .checked_add(Bytes {
                    received: parse(r)?,
                    sent: parse(s)?,
                })
                .map_err(err)?;
        }
        let mut cs = self.conn.prepare_cached("SELECT a,b FROM coverage WHERE res=?1 AND start>=?2 AND start<?3 AND (?4 IS NULL OR network=?4) ORDER BY a,b").map_err(err)?;
        let rows = cs
            .query_map(params![res, from as i64, to as i64, filter], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(err)?;
        // Merge incrementally: retain only disjoint ranges, not every bucket row.
        let mut last: Option<(u64, u64)> = None;
        for row in rows {
            cancelled(cancel)?;
            let (a, b) = row.map_err(err)?;
            let (a, b) = (
                u64::try_from(a).map_err(err)?,
                u64::try_from(b).map_err(err)?,
            );
            if a >= b {
                continue;
            }
            match last.as_mut() {
                Some(range) if a <= range.1 => range.1 = range.1.max(b),
                _ => {
                    if let Some(range) = last.replace((a, b)) {
                        all_coverage.push(range);
                    }
                }
            }
        }
        if let Some(range) = last {
            all_coverage.push(range);
        }
        let coverage = all_coverage;
        let mut gaps = vec![];
        let mut gs = self
            .conn
            .prepare("SELECT at FROM gaps WHERE res=?1 AND at>=?2 AND at<?3")
            .map_err(err)?;
        for at in gs
            .query_map(params![res, from as i64, to as i64], |r| r.get::<_, i64>(0))
            .map_err(err)?
        {
            gaps.push(u64::try_from(at.map_err(err)?).map_err(err)?);
        }
        let mut points = Vec::with_capacity(trend.len() * 3);
        for (a, b, bytes) in trend {
            cancelled(cancel)?;
            let duration = covered(&coverage, a, b)?;
            if duration == 0 {
                points.push(gap_point(a));
                continue;
            }
            let isolated = duration < b - a || gaps.iter().any(|&g| a <= g && g < b);
            if isolated {
                points.push(gap_point(a));
            }
            points.push(HistoryPoint {
                // Isolated rates summarize the bucket, not the location of its
                // observations. Use its midpoint to order both break markers
                // strictly around the visible point without crossing buckets.
                unix_ms: if isolated { a + (b - a) / 2 } else { a },
                download_bps: rate(bytes.received, duration),
                upload_bps: rate(bytes.sent, duration),
                gap: false,
            });
            if isolated {
                points.push(gap_point(b - 1));
            }
        }
        debug_assert!(points.len() <= MAX_POINTS);
        Ok(HistoryResult {
            total,
            covered_ms: covered(&coverage, from, to)?,
            processes: process_totals
                .into_iter()
                .map(|(name, bytes)| ProcessRow { name, bytes })
                .collect(),
            points,
            networks: networks.into_iter().collect(),
        })
    }

    pub fn export_csv(&self, query: HistoryQuery) -> R<String> {
        let result = self.query(query)?;
        let mut out = String::from("\u{feff}kind,name,unix_ms,received_bytes,sent_bytes,covered_ms,download_bps,upload_bps,gap\r\n");
        out.push_str(&format!(
            "total,,,{},{},{},,,\r\n",
            result.total.received, result.total.sent, result.covered_ms
        ));
        for p in result.processes {
            out.push_str(&format!(
                "process,{},,{},{},,,,\r\n",
                csv(&p.name),
                p.bytes.received,
                p.bytes.sent
            ));
        }
        for n in result.networks {
            out.push_str(&format!("network,{},,,,,,,\r\n", csv(&n)));
        }
        for p in result.points {
            out.push_str(&format!(
                "point,,{},,,,{},{},{}\r\n",
                p.unix_ms, p.download_bps, p.upload_bps, p.gap
            ));
        }
        Ok(out)
    }
}
fn apply_record(tx: &Transaction<'_>, unix_ms: u64, record: &IntervalRecord) -> R<()> {
    stamp(unix_ms)?;
    let start = unix_ms
        .checked_sub(record.duration_ms)
        .filter(|_| record.duration_ms > 0)
        .ok_or("invalid interval duration")?;
    // Keep the existing 45s engine guard; do not materialize arbitrary durations.
    if record.duration_ms > MINUTE {
        return Err("interval exceeds one minute".into());
    }
    stamp(start)?;
    if let Some(last) = meta(tx, "last_time")? {
        if start < parse(last)? {
            return Err("interval overlaps previous record or gap".into());
        }
    }
    let network = key(&record.network);
    for res in [0, 1] {
        let mut cursor = start;
        while cursor < unix_ms {
            let (a, b) = bounds(res, cursor)?;
            ensure_bucket(tx, res, a, b, &network)?;
            insert_coverage(tx, res, a, &network, cursor, unix_ms.min(b))?;
            cursor = unix_ms.min(b);
        }
        let (a, b) = bounds(res, unix_ms - 1)?;
        ensure_bucket(tx, res, a, b, &network)?;
        update_bytes(tx, res, a, &network, None, record.physical)?;
        for (name, bytes) in &record.processes {
            update_bytes(tx, res, a, &network, Some(name), *bytes)?;
        }
    }
    put_meta(tx, "last_time", unix_ms)
}
fn apply_gap(tx: &Transaction<'_>, unix_ms: u64) -> R<()> {
    stamp(unix_ms)?;
    if meta(tx, "last_time")?
        .map(parse)
        .transpose()?
        .is_some_and(|last| unix_ms < last)
    {
        return Err("gap precedes previous event".into());
    }
    for res in [0, 1] {
        let (a, _) = bounds(res, unix_ms)?;
        tx.execute("INSERT INTO gaps(res,start,at) VALUES(?1,?2,?3) ON CONFLICT(res,start) DO UPDATE SET at=min(at,excluded.at)", params![res,a as i64,unix_ms as i64]).map_err(err)?;
    }
    put_meta(tx, "last_time", unix_ms)
}

fn gap_point(unix_ms: u64) -> HistoryPoint {
    HistoryPoint {
        unix_ms,
        download_bps: 0.0,
        upload_bps: 0.0,
        gap: true,
    }
}
fn rate(bytes: u64, ms: u64) -> f64 {
    if ms == 0 {
        0.0
    } else {
        bytes as f64 * 1000.0 / ms as f64
    }
}
fn csv(s: &str) -> String {
    // Excel/LibreOffice may ignore leading whitespace before formula markers.
    let dangerous = s
        .trim_start_matches(|c: char| c.is_whitespace() || c.is_control())
        .starts_with(['=', '+', '-', '@']);
    let prefix = if dangerous || s.starts_with(['\t', '\r', '\n']) {
        "'"
    } else {
        ""
    };
    format!("\"{}{}\"", prefix, s.replace('"', "\"\""))
}
fn bounds(res: i32, t: u64) -> R<(u64, u64)> {
    if res == 0 {
        let a = t / MINUTE * MINUTE;
        Ok((a, add(a, MINUTE)?))
    } else {
        day_bounds(t)
    }
}
fn ensure_bucket(tx: &Transaction<'_>, res: i32, a: u64, b: u64, net: &str) -> R<()> {
    tx.execute(
        "INSERT OR IGNORE INTO buckets VALUES(?1,?2,?3,?4,'0','0')",
        params![res, a as i64, b as i64, net],
    )
    .map_err(err)?;
    Ok(())
}
fn insert_coverage(tx: &Transaction<'_>, res: i32, start: u64, net: &str, a: u64, b: u64) -> R<()> {
    let mut stmt = tx
        .prepare("SELECT a,b FROM coverage WHERE res=?1 AND start=?2 AND network=?3")
        .map_err(err)?;
    let rows = stmt
        .query_map(params![res, start as i64, net], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64))
        })
        .map_err(err)?;
    let mut ranges = rows.collect::<Result<Vec<_>, _>>().map_err(err)?;
    ranges.push((a, b));
    tx.execute(
        "DELETE FROM coverage WHERE res=?1 AND start=?2 AND network=?3",
        params![res, start as i64, net],
    )
    .map_err(err)?;
    for (a, b) in ranges_merge(ranges) {
        tx.execute(
            "INSERT INTO coverage VALUES(?1,?2,?3,?4,?5)",
            params![res, start as i64, net, a as i64, b as i64],
        )
        .map_err(err)?;
    }
    Ok(())
}
fn update_bytes(
    tx: &Transaction<'_>,
    res: i32,
    start: u64,
    net: &str,
    name: Option<&str>,
    bytes: Bytes,
) -> R<()> {
    let old: Option<(String,String)> = match name {
        Some(name) => tx.query_row("SELECT received,sent FROM processes WHERE res=?1 AND start=?2 AND network=?3 AND name=?4",params![res,start as i64,net,name],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(err)?,
        None => tx.query_row("SELECT received,sent FROM buckets WHERE res=?1 AND start=?2 AND network=?3",params![res,start as i64,net],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(err)?,
    };
    let old = old
        .map(|(r, s)| {
            Ok::<_, String>(Bytes {
                received: parse(r)?,
                sent: parse(s)?,
            })
        })
        .transpose()?
        .unwrap_or_default();
    let bytes = old.checked_add(bytes).map_err(err)?;
    if let Some(name) = name {
        tx.execute("INSERT INTO processes VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(res,start,network,name) DO UPDATE SET received=excluded.received,sent=excluded.sent",params![res,start as i64,net,name,bytes.received.to_string(),bytes.sent.to_string()]).map_err(err)?;
    } else {
        tx.execute(
            "UPDATE buckets SET received=?4,sent=?5 WHERE res=?1 AND start=?2 AND network=?3",
            params![
                res,
                start as i64,
                net,
                bytes.received.to_string(),
                bytes.sent.to_string()
            ],
        )
        .map_err(err)?;
    }
    Ok(())
}
fn prune(tx: &Transaction<'_>, now: u64) -> R<()> {
    let minute_cutoff = now.saturating_sub(30 * DAY) / MINUTE * MINUTE;
    let date = Local
        .timestamp_millis_opt(stamp(now)?)
        .single()
        .ok_or("invalid date")?
        .date_naive();
    let daily_cutoff = midnight(
        date.checked_sub_days(Days::new(364))
            .ok_or("date overflow")?,
    )?;
    for (res, cutoff) in [(0, minute_cutoff), (1, daily_cutoff)] {
        tx.execute(
            "DELETE FROM buckets WHERE res=?1 AND start<?2",
            params![res, cutoff as i64],
        )
        .map_err(err)?;
        tx.execute(
            "DELETE FROM gaps WHERE res=?1 AND start<?2",
            params![res, cutoff as i64],
        )
        .map_err(err)?;
    }
    put_meta(tx, "minute_cutoff", minute_cutoff)?;
    Ok(())
}
