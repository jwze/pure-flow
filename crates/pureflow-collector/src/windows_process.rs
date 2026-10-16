// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Live handles survive process exit, so delayed ETW records can still resolve birth/name/exit.
use super::lifecycle::{Identity, Lifetimes, MAX_IDENTITIES};
use std::collections::HashMap;
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, FILETIME, HANDLE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                TH32CS_SNAPPROCESS,
            },
            SystemInformation::GetSystemTimeAsFileTime,
            Threading::{
                GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    },
};

struct Process(HANDLE);
// SAFETY: owned process query handles can be used/closed from any thread; accesses are serialized
// by the collector State mutex. No borrowed pointers or thread-affine handles are stored here.
unsafe impl Send for Process {}
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
fn ticks(time: FILETIME) -> u64 {
    ((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64
}
pub(super) fn now() -> u64 {
    ticks(unsafe { GetSystemTimeAsFileTime() })
}
fn times(handle: HANDLE) -> Result<(u64, u64), String> {
    let (mut birth, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    unsafe { GetProcessTimes(handle, &mut birth, &mut exit, &mut kernel, &mut user) }
        .map_err(|_| "process lifetime query unavailable")?;
    Ok((ticks(birth), ticks(exit)))
}
#[derive(Default)]
pub(super) struct Tracker {
    pub lifetimes: Lifetimes,
    handles: HashMap<u32, (u64, Process)>,
    attempts: HashMap<u32, u64>,
}
impl Tracker {
    pub fn capture(&mut self, pid: u32, event_time: Option<u64>) -> Result<(), String> {
        if self.handles.len() >= MAX_IDENTITIES {
            return Err("process handle cache limit exceeded".into());
        }
        let handle = Process(
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                .map_err(|_| "process identity unavailable (exited/protected)")?,
        );
        let (birth, exit) = times(handle.0)?;
        if birth == 0 || event_time.is_some_and(|time| time < birth || (exit != 0 && time > exit)) {
            return Err("ETW event outside opened PID lifetime".into());
        }
        if self.lifetimes.has_birth(pid, birth) {
            if exit != 0 {
                if let Some(old) = self.lifetimes.find(pid, birth).cloned() {
                    self.lifetimes.insert(Identity { exit, ..old });
                }
            }
            return Ok(());
        }
        let mut path = vec![0; 32768];
        let mut len = path.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                handle.0,
                PROCESS_NAME_FORMAT(0),
                PWSTR(path.as_mut_ptr()),
                &mut len,
            )
        }
        .map_err(|_| "process executable name unavailable")?;
        if len == 0 || len as usize > path.len() {
            return Err("process executable name invalid".into());
        }
        let full = String::from_utf16(&path[..len as usize])
            .map_err(|_| "process executable name encoding invalid")?;
        let name = full
            .rsplit(['\\', '/'])
            .next()
            .filter(|v| !v.is_empty())
            .ok_or("process executable name empty")?
            .to_owned();
        if !self.lifetimes.insert(Identity {
            pid,
            birth,
            exit,
            name,
        }) {
            return Err("process lifetime cache limit exceeded".into());
        }
        if exit == 0 {
            self.validate(pid);
            self.handles.insert(pid, (birth, handle));
        }
        Ok(())
    }
    pub fn retry_capture(&mut self, pid: u32, time: u64) {
        let now = now();
        if self
            .attempts
            .get(&pid)
            .is_some_and(|last| now.saturating_sub(*last) < 10_000_000)
        {
            return;
        }
        if self.attempts.len() >= MAX_IDENTITIES {
            return;
        }
        self.attempts.insert(pid, now);
        let _ = self.capture(pid, Some(time));
    }
    pub fn validate(&mut self, pid: u32) {
        // Query cached handles, not OpenProcess: even a lost End must not let an old live
        // cache entry absorb traffic from a reused PID between periodic refreshes.
        if let Some((birth, process)) = self.handles.get(&pid) {
            if let Ok((_, exit)) = times(process.0) {
                if exit != 0 {
                    if let Some(old) = self.lifetimes.find(pid, *birth).cloned() {
                        self.lifetimes.insert(Identity { exit, ..old });
                    }
                    self.handles.remove(&pid);
                }
            }
        }
    }
    pub fn start(&mut self, pid: u32, birth: u64, name: String) -> Result<(), String> {
        self.validate(pid);
        // If still alive, use exact creation FILETIME + handle. Otherwise the actual ETW Start
        // payload name and timestamp supply identity; never manufacture a PID display name.
        if self.capture(pid, Some(birth)).is_ok() {
            return Ok(());
        }
        if self.lifetimes.insert(Identity {
            pid,
            birth,
            exit: 0,
            name,
        }) {
            Ok(())
        } else {
            Err("process start identity/cache invalid".into())
        }
    }
    pub fn end(&mut self, pid: u32, time: u64) {
        self.validate(pid);
        self.lifetimes.end(pid, time);
    }
    pub fn refresh(&mut self) {
        self.handles.retain(|pid, (birth, process)| {
            if let Ok((_, exit)) = times(process.0) {
                if exit != 0 {
                    if let Some(old) = self.lifetimes.find(*pid, *birth).cloned() {
                        self.lifetimes.insert(Identity { exit, ..old });
                    }
                    return false; // Exact exit/name retained; OS handle no longer needed.
                }
            }
            true
        });
        let now = now();
        self.lifetimes.prune(now);
        self.attempts
            .retain(|_, last| now.saturating_sub(*last) < 10_000_000);
    }
    pub fn snapshot(&mut self) -> Result<(), String> {
        let snapshot = Process(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
                .map_err(|e| format!("Toolhelp initial process snapshot failed: {e}"))?,
        );
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        unsafe { Process32FirstW(snapshot.0, &mut entry) }
            .map_err(|e| format!("Toolhelp initial process enumeration failed: {e}"))?;
        loop {
            // Individual access-denied/racing exits are normal. Only usable names + births enter
            // the cache; a later unresolved network event is explicitly reported as omitted.
            if entry.th32ProcessID != 0 {
                let _ = self.capture(entry.th32ProcessID, None);
            }
            if let Err(error) = unsafe { Process32NextW(snapshot.0, &mut entry) } {
                if error.code() != windows::core::HRESULT::from_win32(ERROR_NO_MORE_FILES.0) {
                    return Err(format!("Toolhelp enumeration interrupted: {error}"));
                }
                break;
            }
        }
        Ok(())
    }
}
