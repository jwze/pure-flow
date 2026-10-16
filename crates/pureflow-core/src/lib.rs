// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Shared, platform-independent counters. Never derive machine totals by summing processes.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

/// JSON byte counters are decimal strings so a WebView cannot lose u64 precision.
mod decimal_u64 {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(D::Error::custom("expected an unsigned decimal string"));
        }
        text.parse().map_err(D::Error::custom)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bytes {
    #[serde(with = "decimal_u64")]
    pub received: u64,
    #[serde(with = "decimal_u64")]
    pub sent: u64,
}
impl Bytes {
    pub fn checked_add(self, other: Self) -> Result<Self, AccountingError> {
        Ok(Self {
            received: self
                .received
                .checked_add(other.received)
                .ok_or(AccountingError::Overflow)?,
            sent: self
                .sent
                .checked_add(other.sent)
                .ok_or(AccountingError::Overflow)?,
        })
    }
    /// Each reset direction independently contributes zero, never unsigned wraparound.
    pub fn difference(self, previous: Self) -> Self {
        Self {
            received: self.received.saturating_sub(previous.received),
            sent: self.sent.saturating_sub(previous.sent),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CounterMode {
    Cumulative,
    Interval,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceCounter {
    pub id: String,
    pub name: String,
    pub bytes: Bytes,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessCounter {
    /// Platform-specific PID + process birth identity, not just the display name or PID.
    pub identity: String,
    pub pid: u32,
    pub name: String,
    pub bytes: Bytes,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSample {
    pub interfaces: Vec<InterfaceCounter>,
    pub processes: Vec<ProcessCounter>,
    pub process_mode: CounterMode,
    /// Actual SSID if available. None means all-network statistics only.
    pub network: Option<String>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntervalRecord {
    pub duration_ms: u64,
    pub physical: Bytes,
    pub processes: BTreeMap<String, Bytes>,
    pub network: Option<String>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AccountingError {
    #[error("counter aggregation exceeds u64 range")]
    Overflow,
    #[error("sample contains duplicate {0} identity")]
    DuplicateIdentity(&'static str),
    #[error("sample contains an empty {0} identity")]
    EmptyIdentity(&'static str),
    #[error("monotonic sample time must increase")]
    NonIncreasingTime,
}

/// Monotonic time is supplied by the host, never computed from the wall clock.
/// Invalid samples are rejected transactionally without destroying the previous baseline.
#[derive(Debug, Default)]
pub struct CounterEngine {
    previous: Option<(u64, RawSample)>,
}
impl CounterEngine {
    pub fn reset(&mut self) {
        self.previous = None;
    }

    pub fn apply(
        &mut self,
        monotonic_ms: u64,
        sample: RawSample,
    ) -> Result<Option<IntervalRecord>, AccountingError> {
        validate(&sample)?;
        let Some((old_ms, old)) = &self.previous else {
            self.previous = Some((monotonic_ms, sample));
            return Ok(None);
        };
        let elapsed = monotonic_ms
            .checked_sub(*old_ms)
            .filter(|value| *value > 0)
            .ok_or(AccountingError::NonIncreasingTime)?;
        // A missing interval, network switch, or adapter set change must not be charged to
        // the new context. The 45 second long-gap guard is independent of the platform's display cadence.
        let interfaces_changed = sample
            .interfaces
            .iter()
            .map(|v| &v.id)
            .collect::<std::collections::BTreeSet<_>>()
            != old.interfaces.iter().map(|v| &v.id).collect();
        if elapsed >= 45_000
            || sample.network != old.network
            || interfaces_changed
            || sample.process_mode != old.process_mode
        {
            self.previous = Some((monotonic_ms, sample));
            return Ok(None);
        }
        let old_interfaces: HashMap<_, _> =
            old.interfaces.iter().map(|v| (&v.id, v.bytes)).collect();
        let mut physical = Bytes::default();
        let mut active_interfaces = 0;
        for value in &sample.interfaces {
            if let Some(previous) = old_interfaces.get(&value.id) {
                let delta = value.bytes.difference(*previous);
                if delta.received > 0 || delta.sent > 0 {
                    active_interfaces += 1;
                }
                physical = physical.checked_add(delta)?;
            }
        }
        let old_processes: HashMap<_, _> = old.processes.iter().map(|v| (&v.identity, v)).collect();
        let mut processes = BTreeMap::<String, Bytes>::new();
        for value in &sample.processes {
            let delta = match sample.process_mode {
                CounterMode::Interval => value.bytes,
                CounterMode::Cumulative => {
                    let Some(previous) = old_processes.get(&value.identity) else {
                        continue;
                    };
                    if value.pid != previous.pid || value.name != previous.name {
                        continue;
                    }
                    value.bytes.difference(previous.bytes)
                }
            };
            if delta.received > 0 || delta.sent > 0 {
                let current = processes.get(&value.name).copied().unwrap_or_default();
                processes.insert(value.name.clone(), current.checked_add(delta)?);
            }
        }
        let record = IntervalRecord {
            duration_ms: elapsed,
            physical,
            processes,
            network: if active_interfaces > 1 {
                None
            } else {
                sample.network.clone()
            },
            warnings: sample.warnings.clone(),
        };
        self.previous = Some((monotonic_ms, sample));
        Ok(Some(record))
    }
}

fn validate(sample: &RawSample) -> Result<(), AccountingError> {
    let mut interfaces = std::collections::HashSet::new();
    for value in &sample.interfaces {
        if value.id.is_empty() {
            return Err(AccountingError::EmptyIdentity("interface"));
        }
        if !interfaces.insert(&value.id) {
            return Err(AccountingError::DuplicateIdentity("interface"));
        }
    }
    let mut processes = std::collections::HashSet::new();
    for value in &sample.processes {
        if value.identity.is_empty() {
            return Err(AccountingError::EmptyIdentity("process"));
        }
        if !processes.insert(&value.identity) {
            return Err(AccountingError::DuplicateIdentity("process"));
        }
    }
    Ok(())
}
