// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Pure, bounded PID lifetime index. Timestamps are FILETIME ticks, never receipt time.
use std::collections::{HashMap, VecDeque};

pub(super) const MAX_IDENTITIES: usize = 4096;
const RETAIN_TICKS: u64 = 120 * 10_000_000;

#[derive(Clone, Debug)]
pub(super) struct Identity {
    pub pid: u32,
    pub birth: u64,
    pub exit: u64,
    pub name: String,
}
impl Identity {
    pub fn contains(&self, time: u64) -> bool {
        i64::try_from(time)
            .is_ok_and(|time| crate::windows_schema::event_in_lifetime(time, self.birth, self.exit))
    }
}
#[derive(Default)]
pub(super) struct Lifetimes {
    entries: HashMap<u32, Vec<Identity>>,
    // End may be delivered before start (different ETW buffers/CPUs).
    ends: VecDeque<(u32, u64)>,
    count: usize,
    strings: usize,
}
impl Lifetimes {
    pub fn insert(&mut self, mut identity: Identity) -> bool {
        if identity.pid == 0 || identity.birth == 0 || identity.name.is_empty() {
            return false;
        }
        if identity.name.len() > 32768 {
            return false;
        }
        if let Some(items) = self.entries.get_mut(&identity.pid) {
            let next_birth = items
                .iter()
                .filter(|item| item.birth > identity.birth)
                .map(|item| item.birth)
                .min();
            if let Some(old) = items.iter_mut().find(|item| item.birth == identity.birth) {
                if identity.exit != 0 {
                    // A precise late handle refresh may replace an estimated
                    // exit, but never cross a newer PID generation's birth.
                    old.exit = next_birth.map_or(identity.exit, |next| identity.exit.min(next - 1));
                }
                return true;
            }
        }
        if self.count >= MAX_IDENTITIES
            || self.strings.saturating_add(identity.name.len()) > 8 * 1024 * 1024
        {
            return false;
        }
        if identity.exit == 0 {
            identity.exit = self
                .ends
                .iter()
                .filter(|(pid, end)| *pid == identity.pid && *end >= identity.birth)
                .map(|(_, end)| *end)
                .min()
                .unwrap_or(0);
        }
        let items = self.entries.entry(identity.pid).or_default();
        // A later birth is an upper bound on an earlier generation, even if End was lost.
        if let Some(next) = items
            .iter()
            .filter(|v| v.birth > identity.birth)
            .map(|v| v.birth)
            .min()
        {
            identity.exit = if identity.exit == 0 {
                next - 1
            } else {
                identity.exit.min(next - 1)
            };
        }
        for old in items.iter_mut().filter(|v| v.birth < identity.birth) {
            if old.exit == 0 || old.exit >= identity.birth {
                old.exit = identity.birth - 1;
            }
        }
        self.strings += identity.name.len();
        items.push(identity);
        items.sort_unstable_by_key(|v| v.birth);
        self.count += 1;
        true
    }
    pub fn find(&self, pid: u32, time: u64) -> Option<&Identity> {
        if time == 0 {
            return None;
        }
        self.entries
            .get(&pid)?
            .iter()
            .rev()
            .find(|v| v.contains(time))
    }
    pub fn end(&mut self, pid: u32, time: u64) {
        if time == 0 || pid == 0 {
            return;
        }
        if let Some(identity) = self
            .entries
            .get_mut(&pid)
            .and_then(|items| items.iter_mut().rev().find(|v| v.contains(time)))
        {
            // Tighten a synthetic next-birth upper bound too. End must never extend an
            // exact exit; a later handle refresh may replace this with exact FILETIME.
            identity.exit = if identity.exit == 0 {
                time
            } else {
                identity.exit.min(time)
            };
        }
        if self.ends.len() == MAX_IDENTITIES {
            self.ends.pop_front();
        }
        self.ends.push_back((pid, time));
    }
    pub fn prune(&mut self, now: u64) {
        self.entries.retain(|_, items| {
            items.retain(|v| v.exit == 0 || now.saturating_sub(v.exit) <= RETAIN_TICKS);
            !items.is_empty()
        });
        self.count = self.entries.values().map(Vec::len).sum();
        self.strings = self.entries.values().flatten().map(|v| v.name.len()).sum();
        self.ends
            .retain(|(_, end)| now.saturating_sub(*end) <= RETAIN_TICKS);
    }
    pub fn has_birth(&self, pid: u32, birth: u64) -> bool {
        self.entries
            .get(&pid)
            .is_some_and(|v| v.iter().any(|v| v.birth == birth))
    }
}
