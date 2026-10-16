// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
use pureflow_core::{Bytes, IntervalRecord};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Serialize)]
pub struct ProcessRow {
    pub name: String,
    pub bytes: Bytes,
}
#[derive(Clone, Serialize)]
pub struct Point {
    pub unix_ms: u64,
    pub download_bps: f64,
    pub upload_bps: f64,
    pub gap: bool,
}
#[derive(Clone, Serialize)]
pub struct Snapshot {
    pub phase: String,
    pub paused: bool,
    pub status: String,
    pub started_at_ms: u64,
    pub last_sample_ms: Option<u64>,
    pub covered_ms: u64,
    pub total: Bytes,
    pub download_bps: f64,
    pub upload_bps: f64,
    pub processes: Vec<ProcessRow>,
    pub points: Vec<Point>,
    pub warnings: Vec<String>,
}

pub struct SessionModel {
    pub snapshot: Snapshot,
    processes: BTreeMap<String, Bytes>,
    points: VecDeque<Point>,
}
pub struct Prepared {
    total: Bytes,
    covered_ms: u64,
    updates: Vec<(String, Bytes)>,
    omitted: bool,
}
impl SessionModel {
    pub fn new(unix_ms: u64) -> Self {
        Self {
            snapshot: Snapshot {
                phase: "starting".into(),
                paused: false,
                status: "正在启动本机采集…".into(),
                started_at_ms: unix_ms,
                last_sample_ms: None,
                covered_ms: 0,
                total: Bytes::default(),
                download_bps: 0.0,
                upload_bps: 0.0,
                processes: Vec::new(),
                points: Vec::new(),
                warnings: Vec::new(),
            },
            processes: BTreeMap::new(),
            points: VecDeque::new(),
        }
    }
    pub fn gap(&mut self, unix_ms: u64) {
        self.snapshot.download_bps = 0.0;
        self.snapshot.upload_bps = 0.0;
        if self.points.back().is_none_or(|point| !point.gap) {
            self.add_point(Point {
                unix_ms,
                download_bps: 0.0,
                upload_bps: 0.0,
                gap: true,
            });
        }
    }
    fn add_point(&mut self, point: Point) {
        self.points.push_back(point);
        while self.points.len() > 120 {
            self.points.pop_front();
        }
    }
    /// Materialize large collections only when a changed snapshot is requested.
    pub fn materialize(&self) -> Snapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.processes = self
            .processes
            .iter()
            .map(|(name, bytes)| ProcessRow {
                name: name.clone(),
                bytes: *bytes,
            })
            .collect();
        snapshot.points = self.points.iter().cloned().collect();
        snapshot
    }
    pub fn prepare(&self, record: &IntervalRecord) -> Result<Prepared, String> {
        if record.duration_ms == 0 {
            return Err("采样时长为零，已停止记录".into());
        }
        let total = self
            .snapshot
            .total
            .checked_add(record.physical)
            .map_err(|e| e.to_string())?;
        let covered_ms = self
            .snapshot
            .covered_ms
            .checked_add(record.duration_ms)
            .ok_or("覆盖时长溢出")?;
        let mut updates = Vec::with_capacity(record.processes.len());
        let mut new_names = 0;
        let mut omitted = false;
        for (name, bytes) in &record.processes {
            if !self.processes.contains_key(name) {
                if self.processes.len() + new_names >= 4096 {
                    omitted = true;
                    continue;
                }
                new_names += 1;
            }
            let previous = self.processes.get(name).copied().unwrap_or_default();
            updates.push((
                name.clone(),
                previous.checked_add(*bytes).map_err(|e| e.to_string())?,
            ));
        }
        Ok(Prepared {
            total,
            covered_ms,
            updates,
            omitted,
        })
    }
    /// Called after durable commit (Windows) or bounded writer acceptance (macOS); no fallible arithmetic remains.
    pub fn commit(&mut self, unix_ms: u64, record: IntervalRecord, prepared: Prepared) {
        for (name, bytes) in prepared.updates {
            self.processes.insert(name, bytes);
        }
        self.snapshot.total = prepared.total;
        self.snapshot.covered_ms = prepared.covered_ms;
        let seconds = record.duration_ms as f64 / 1000.0;
        self.snapshot.download_bps = record.physical.received as f64 / seconds;
        self.snapshot.upload_bps = record.physical.sent as f64 / seconds;
        self.snapshot.last_sample_ms = Some(unix_ms);
        self.snapshot.phase = "running".into();
        self.snapshot.status = crate::sampling_policy::RUNNING.into();
        self.snapshot.warnings = record.warnings;
        if prepared.omitted {
            self.snapshot
                .warnings
                .push("本次运行进程名称数超过 4096，新名称已忽略；整机总量不受影响".into());
        }
        self.add_point(Point {
            unix_ms,
            download_bps: self.snapshot.download_bps,
            upload_bps: self.snapshot.upload_bps,
            gap: false,
        });
    }
}
