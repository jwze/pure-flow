// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Private system ETW session + independent physical interface counters.
//! This prototype needs elevated tracing privileges, not an elevated WebView.
use crate::{
    windows_schema::{self, Direction},
    CollectorError,
};
use pureflow_core::{Bytes, CounterMode, InterfaceCounter, ProcessCounter, RawSample};
use std::{
    collections::{HashMap, VecDeque},
    ffi::c_void,
    mem::{align_of, size_of},
    ptr::{addr_of, null_mut},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};
use windows::{
    core::{GUID, PCWSTR, PWSTR},
    Win32::{
        Foundation::{GetLastError, ERROR_SUCCESS},
        NetworkManagement::{
            IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2},
            Ndis::IfOperStatusUp,
        },
        System::{Diagnostics::Etw::*, Threading::GetCurrentProcessId},
    },
};

const MAX_PENDING: usize = 16_384;
const MAX_PENDING_WIRE_BUDGET: usize = 6 * 1024 * 1024;
const MAX_WARNINGS: usize = 32;
#[path = "windows_etw.rs"]
mod etw;
#[path = "windows_lifecycle.rs"]
mod lifecycle;
#[path = "windows_owners.rs"]
mod owners;
#[path = "windows_process.rs"]
mod process;
#[path = "windows_wlan.rs"]
mod wlan;
const MAX_UNRESOLVED: usize = 16_384;
const UNRESOLVED_TICKS: u64 = 30 * 10_000_000;
const UNRESOLVED_SWEEP_TICKS: u64 = 10 * 10_000_000;
static SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

fn unavailable(value: impl ToString) -> CollectorError {
    CollectorError::Unavailable(value.to_string())
}

/// FFI structs need alignment, not Vec<u8>'s alignment-one allocation guarantee.
struct AlignedBuffer {
    words: Vec<u64>,
    len: usize,
}
impl AlignedBuffer {
    fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(8)],
            len,
        }
    }
    fn ptr<T>(&mut self) -> *mut T {
        assert!(align_of::<T>() <= align_of::<u64>());
        self.words.as_mut_ptr().cast()
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: the words allocation covers len bytes, initialized to zero.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast(), self.len) }
    }
}
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct ProcessKey {
    pid: u32,
    birth: u64,
}
struct PendingCounter {
    name: String,
    bytes: Bytes,
}
#[derive(Default)]
struct State {
    pending: HashMap<ProcessKey, PendingCounter>,
    pending_strings: usize,
    warnings: Vec<String>,
    decoded_total: u64,
    omitted: u64,
    consumer_error: Option<String>,
    tracker: process::Tracker,
    decoder: etw::Decoder,
    unresolved: HashMap<u32, VecDeque<NetworkEvent>>,
    unresolved_count: usize,
    last_unresolved_sweep: u64,
    last_refresh: u64,
}
impl State {
    fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        if self.warnings.len() < MAX_WARNINGS && !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
        self.omitted = self.omitted.saturating_add(1);
    }
}

pub struct WindowsCollector {
    state: Arc<Mutex<State>>,
    session: Session,
    last_losses: u64,
}
impl WindowsCollector {
    pub fn start() -> Result<Self, CollectorError> {
        let state = Arc::new(Mutex::new(State::default()));
        let session = Session::start(Arc::clone(&state))?;
        Ok(Self {
            state,
            session,
            last_losses: 0,
        })
    }
    pub fn shutdown(&mut self) -> Result<(), CollectorError> {
        self.session.stop()
    }
    pub fn sample(&mut self) -> Result<RawSample, CollectorError> {
        let (interfaces, network, mut interface_warnings) = interfaces()?;
        let losses = self.session.losses()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| unavailable("ETW state lock poisoned"))?;
        if let Some(error) = &state.consumer_error {
            return Err(unavailable(error));
        }
        state.tracker.refresh();
        // Samples drive maintenance even if no further ETW/network records arrive.
        state.sweep_unresolved(process::now());
        let mut warnings = std::mem::take(&mut state.warnings);
        warnings.append(&mut interface_warnings);
        if state.decoded_total == 0 {
            warnings.push("ETW has not decoded any process network event yet; an empty list does not establish protocol coverage".into());
        }
        if state.unresolved_count != 0 {
            warnings.push(format!("{} ETW network records await delayed process identity (30s retention, swept every 10s); not yet included in this interval", state.unresolved_count));
        }
        warnings.push("ETW arrival and interface snapshot boundaries can differ; process bytes are not physical totals".into());
        warnings.push("ETW process identity retention is bounded (120s after exit); lost lifecycle events, protected processes and unsupported schemas/names can still omit traffic; short-process coverage requires real Windows validation".into());
        if losses > self.last_losses {
            warnings.push(format!(
                "ETW reported {} additional lost events/buffers",
                losses - self.last_losses
            ));
        }
        self.last_losses = losses;
        if state.omitted > 0 {
            warnings.push(format!(
                "{} ETW records encountered decode/identity/limit problems this interval",
                state.omitted
            ));
        }
        state.omitted = 0;
        let processes = state
            .pending
            .drain()
            .map(|(key, counter)| ProcessCounter {
                identity: format!("windows:{}:{}", key.pid, key.birth),
                pid: key.pid,
                name: counter.name,
                bytes: counter.bytes,
            })
            .collect();
        state.pending_strings = 0;
        // Always drain, including the engine's first baseline/reset sample.
        Ok(RawSample {
            interfaces,
            processes,
            process_mode: CounterMode::Interval,
            network,
            warnings,
        })
    }
}

struct Session {
    control: Option<CONTROLTRACE_HANDLE>,
    consumer: Option<PROCESSTRACE_HANDLE>,
    // Keep callback context/name alive through CloseTrace + consumer thread join.
    _state: Arc<Mutex<State>>,
    _name: Vec<u16>,
    properties: AlignedBuffer,
    owner: owners::Owner,
    owner_removed: bool,
    thread: Option<JoinHandle<()>>,
    join_panicked: bool,
}
impl Session {
    fn start(state: Arc<Mutex<State>>) -> Result<Self, CollectorError> {
        let sequence = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = unsafe { GetCurrentProcessId() };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(unavailable)?
            .as_nanos();
        let guid = GUID::from_u128((nonce << 32) ^ ((pid as u128) << 64) ^ sequence as u128);
        let (mut owner, name) = owners::Owner::prepare(guid)?;
        let mut name_w: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let total = size_of::<EVENT_TRACE_PROPERTIES>() + name_w.len() * 2;
        let mut properties = AlignedBuffer::new(total);
        let p = properties.ptr::<EVENT_TRACE_PROPERTIES>();
        // SAFETY: aligned, zero-initialized allocation includes header and UTF-16 name.
        unsafe {
            (*p).Wnode.BufferSize = total as u32;
            (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*p).Wnode.ClientContext = 1; // QPC; ProcessTrace converts to FILETIME (RAW_TIMESTAMP is not set).
            (*p).Wnode.Guid = guid;
            (*p).BufferSize = 64;
            (*p).MinimumBuffers = 4;
            (*p).MaximumBuffers = 64;
            (*p).FlushTimer = 1;
            // Private NAME, not PRIVATE_LOGGER_MODE (which is a different user-mode facility).
            (*p).LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE;
            (*p).EnableFlags = EVENT_TRACE_FLAG_NETWORK_TCPIP | EVENT_TRACE_FLAG_PROCESS;
            (*p).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            std::ptr::copy_nonoverlapping(
                name_w.as_ptr().cast::<u8>(),
                properties
                    .words
                    .as_mut_ptr()
                    .cast::<u8>()
                    .add(size_of::<EVENT_TRACE_PROPERTIES>()),
                name_w.len() * 2,
            );
        }
        let mut control = CONTROLTRACE_HANDLE::default();
        let rc = unsafe { StartTraceW(&mut control, PCWSTR(name_w.as_ptr()), p) };
        if rc != ERROR_SUCCESS {
            let mut message = format!("StartTraceW failed ({}): approve the GUI's background collector UAC request; trace permissions/session slots must be available", rc.0);
            if let Err(error) = owner.remove() {
                message.push_str(&format!("; owner cleanup failed: {error}"));
            }
            return Err(unavailable(message));
        }
        let mut session = Self {
            control: Some(control),
            consumer: None,
            _state: Arc::clone(&state),
            _name: Vec::new(),
            properties,
            owner,
            owner_removed: false,
            thread: None,
            join_panicked: false,
        };
        // StartTrace flags enable the legacy system provider. Do not EnableTraceEx2 on TcpIpGuid.
        let mut logfile = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name_w.as_mut_ptr()),
            Context: Arc::as_ptr(&state).cast_mut().cast(),
            ..Default::default()
        };
        logfile.Anonymous1 = EVENT_TRACE_LOGFILEW_0 {
            ProcessTraceMode: PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_REAL_TIME,
        };
        logfile.Anonymous2 = EVENT_TRACE_LOGFILEW_1 {
            EventRecordCallback: Some(callback),
        };
        let consumer = unsafe { OpenTraceW(&mut logfile) };
        if consumer.Value == u64::MAX {
            let error = unsafe { GetLastError() };
            return Err(session.start_failure(format!("OpenTraceW failed: {}", error.0)));
        }
        session.consumer = Some(consumer);
        // Retain the exact allocation supplied to OpenTrace through CloseTrace + join.
        session._name = name_w;
        // Session is enabled before snapshot: starts/exits during enumeration remain in ETW.
        // Populate before spawning the consumer, so no callback races the initial capture.
        if let Ok(mut state) = state.lock() {
            if let Err(error) = state.tracker.snapshot() {
                state.warn(error);
            }
        }
        let thread_state = Arc::clone(&state);
        let thread = match thread::Builder::new()
            .name("pureflow-etw".into())
            .spawn(move || {
                let result = unsafe { ProcessTrace(&[consumer], None, None) };
                if let Ok(mut state) = thread_state.lock() {
                    // Closing the owned trace during Drop also ends ProcessTrace, but no sample
                    // is requested then. Any spontaneous completion is an unavailable collector.
                    state.consumer_error = Some(format!("ETW consumer ended: {}", result.0));
                }
            }) {
            Ok(thread) => thread,
            Err(error) => return Err(session.start_failure(error)),
        };
        session.thread = Some(thread);
        Ok(session)
    }
    fn start_failure(&mut self, error: impl ToString) -> CollectorError {
        match self.stop() {
            Ok(()) => unavailable(error),
            Err(cleanup) => unavailable(format!(
                "{}; ETW startup cleanup failed: {cleanup}",
                error.to_string()
            )),
        }
    }
    fn losses(&mut self) -> Result<u64, CollectorError> {
        let control = self
            .control
            .ok_or_else(|| unavailable("ETW session stopped"))?;
        let p = self.properties.ptr::<EVENT_TRACE_PROPERTIES>();
        let status =
            unsafe { ControlTraceW(control, PCWSTR::null(), p, EVENT_TRACE_CONTROL_QUERY) };
        if status != ERROR_SUCCESS {
            return Err(unavailable(format!(
                "ETW session query failed: {}",
                status.0
            )));
        }
        // SAFETY: successful query populated the aligned header.
        unsafe {
            Ok((*p).EventsLost as u64
                + (*p).LogBuffersLost as u64
                + (*p).RealTimeBuffersLost as u64)
        }
    }
    fn stop(&mut self) -> Result<(), CollectorError> {
        let mut errors = Vec::new();
        if let Some(control) = self.control {
            // Only our private handle, never NT Kernel Logger or a prefix/name lookup.
            let status = unsafe {
                ControlTraceW(
                    control,
                    PCWSTR::null(),
                    self.properties.ptr(),
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            if status == ERROR_SUCCESS || status.0 == 4201 {
                self.control = None;
            } else {
                errors.push(format!(
                    "ETW stop failed ({}); exact owner record retained for recovery",
                    status.0
                ));
            }
        }
        if let Some(consumer) = self.consumer {
            let status = unsafe { CloseTrace(consumer) };
            // ERROR_CTX_CLOSE_PENDING is documented success for a real-time consumer;
            // ProcessTrace finishes processing its current buffer before returning.
            if status == ERROR_SUCCESS || status.0 == 7007 {
                self.consumer = None;
            } else {
                errors.push(format!("ETW CloseTrace failed ({})", status.0));
            }
        }
        // Either successful stop or close ends ProcessTrace. If both failed, do not
        // block forever waiting for a still-live session; callers receive the errors.
        if self.control.is_none() || self.consumer.is_none() {
            if let Some(thread) = self.thread.take() {
                self.join_panicked = thread.join().is_err();
            }
        }
        if self.join_panicked {
            errors.push("ETW consumer panicked during shutdown".into());
        }
        if self.control.is_none() && !self.owner_removed {
            match self.owner.remove() {
                Ok(()) => self.owner_removed = true,
                Err(error) => errors.push(format!("ETW owner journal cleanup failed: {error}")),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(unavailable(errors.join("; ")))
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("PureFlow ETW shutdown: {error}");
        }
        if self.thread.is_some() {
            // STOP and CloseTrace both failed: the consumer may still call back.
            // Preserve its backing allocations rather than risk a use-after-free.
            let _ = Arc::into_raw(Arc::clone(&self._state));
            std::mem::forget(std::mem::take(&mut self._name));
        }
    }
}

type InterfaceSample = (Vec<InterfaceCounter>, Option<String>, Vec<String>);
fn interfaces() -> Result<InterfaceSample, CollectorError> {
    let mut table: *mut MIB_IF_TABLE2 = null_mut();
    let status = unsafe { GetIfTable2(&mut table) };
    if status != ERROR_SUCCESS {
        return Err(unavailable(format!("GetIfTable2 failed: {}", status.0)));
    }
    if table.is_null() {
        return Err(unavailable("GetIfTable2 returned null"));
    }
    struct Table(*mut MIB_IF_TABLE2);
    impl Drop for Table {
        fn drop(&mut self) {
            unsafe {
                FreeMibTable(self.0.cast::<c_void>());
            }
        }
    }
    let table = Table(table);
    // SAFETY: IP Helper allocates NumEntries contiguous rows after the table header.
    let rows = unsafe {
        std::slice::from_raw_parts(
            addr_of!((*table.0).Table).cast::<MIB_IF_ROW2>(),
            (*table.0).NumEntries as usize,
        )
    };
    let counters: Vec<_> = rows
        .iter()
        .filter(|row| physical_up(row))
        .map(|row| InterfaceCounter {
            id: format!("windows:{:?}", row.InterfaceGuid),
            name: String::from_utf16_lossy(
                &row.Alias[..row
                    .Alias
                    .iter()
                    .position(|v| *v == 0)
                    .unwrap_or(row.Alias.len())],
            ),
            bytes: Bytes {
                received: row.InOctets,
                sent: row.OutOctets,
            },
        })
        .collect();
    let mut warnings = Vec::new();
    if counters.is_empty() {
        warnings.push("no active hardware Ethernet/Wi-Fi interface counters; sampling continues and will recover when an interface becomes active".into());
    }
    let physical: Vec<_> = rows
        .iter()
        .filter(|row| physical_up(row))
        .map(|row| (row.InterfaceGuid, row.Type))
        .collect();
    let network = wlan::network(&physical, &mut warnings);
    Ok((counters, network, warnings))
}
fn physical_up(row: &MIB_IF_ROW2) -> bool {
    // HardwareInterface is bit 0; IEEE80211=71, Ethernet=6. MAC presence alone includes VPNs.
    row.OperStatus == IfOperStatusUp
        && row.InterfaceAndOperStatusFlags._bitfield & 1 != 0
        && matches!(row.Type, 6 | 71)
}

unsafe extern "system" fn callback(pointer: *mut EVENT_RECORD) {
    if pointer.is_null() {
        return;
    }
    // SAFETY: ETW owns the event for the duration of this call. Context is kept alive by Session.
    let record = unsafe { &*pointer };
    if record.UserContext.is_null() {
        return;
    }
    let state = unsafe { &*record.UserContext.cast::<Mutex<State>>() };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_event(record, state)));
    if result.is_err() {
        if let Ok(mut state) = state.lock() {
            state.warn("ETW callback panicked; event omitted");
        }
    }
}
#[derive(Clone, Copy)]
struct NetworkEvent {
    pid: u32,
    time: u64,
    received: u64,
    bytes: Bytes,
}
impl State {
    fn account(&mut self, event: NetworkEvent) -> bool {
        self.tracker.validate(event.pid);
        let Some(identity) = self.tracker.lifetimes.find(event.pid, event.time) else {
            return false;
        };
        let key = ProcessKey {
            pid: identity.pid,
            birth: identity.birth,
        };
        if let Some(previous) = self.pending.get_mut(&key) {
            match previous.bytes.checked_add(event.bytes) {
                Ok(bytes) => previous.bytes = bytes,
                Err(_) => self.warn("ETW byte counter overflow; event omitted"),
            }
        } else {
            // Budget worst-case JSON escaping (6 bytes per input byte) plus
            // identity, field names and full u64 counters. Leave 2 MiB of the
            // helper's 8 MiB frame for interfaces/warnings instead of failing IPC.
            // No formatting/allocation is needed on the network-event path.
            let cost = identity.name.len().saturating_mul(6).saturating_add(256);
            if self.pending.len() >= MAX_PENDING
                || self.pending_strings.saturating_add(cost) > MAX_PENDING_WIRE_BUDGET
            {
                self.warn("ETW pending identity/string limit exceeded; event omitted");
                return true;
            }
            let counter = PendingCounter {
                name: identity.name.clone(),
                bytes: event.bytes,
            };
            self.pending_strings += cost;
            self.pending.insert(key, counter);
        }
        true
    }
    fn resolve_events(&mut self, pid: u32, mut events: VecDeque<NetworkEvent>, now: u64) {
        self.unresolved_count -= events.len();
        events.retain(|event| {
            // Receipt time bounds retention; event time alone selects the PID generation.
            // Never attribute an expired record just because identity arrived late.
            if now.saturating_sub(event.received) >= UNRESOLVED_TICKS {
                self.warn("ETW process identity unresolved after 30s; network event omitted (lost Start/unsupported name/protected/exited process)");
                false
            } else {
                !self.account(*event)
            }
        });
        if !events.is_empty() {
            self.unresolved_count += events.len();
            self.unresolved.insert(pid, events);
        }
    }
    fn resolve_pid(&mut self, pid: u32, now: u64) {
        if let Some(events) = self.unresolved.remove(&pid) {
            self.resolve_events(pid, events, now);
        }
    }
    fn sweep_unresolved(&mut self, now: u64) {
        if now.saturating_sub(self.last_unresolved_sweep) < UNRESOLVED_SWEEP_TICKS {
            return;
        }
        self.last_unresolved_sweep = now;
        // Global work is sample-driven and throttled, never triggered by an unrelated
        // lifecycle event. Also retry identities learned through periodic handle refresh.
        let unresolved = std::mem::take(&mut self.unresolved);
        for (pid, events) in unresolved {
            self.resolve_events(pid, events, now);
        }
    }
    fn defer(&mut self, event: NetworkEvent) {
        if self.unresolved_count >= MAX_UNRESOLVED {
            self.warn("ETW unresolved network queue limit exceeded; event omitted");
            return;
        }
        self.unresolved
            .entry(event.pid)
            .or_default()
            .push_back(event);
        self.unresolved_count += 1;
    }
}
fn on_event(record: &EVENT_RECORD, state: &Mutex<State>) {
    let provider = record.EventHeader.ProviderId;
    let opcode = record.EventHeader.EventDescriptor.Opcode;
    let process_event = provider == ProcessGuid && matches!(opcode, 1 | 2);
    let direction = if provider == TcpIpGuid || provider == UdpIpGuid {
        windows_schema::direction(opcode)
    } else {
        None
    };
    // DCStart/DCEnd are rundown, not birth/exit: snapshot handles supply exact times instead.
    if !process_event && direction.is_none() {
        return;
    }
    let Ok(mut state) = state.lock() else {
        return;
    };
    if record.EventHeader.TimeStamp <= 0 || record.UserData.is_null() || record.UserDataLength == 0
    {
        state.warn("ETW event missing valid FILETIME timestamp/payload; event omitted");
        return;
    }
    let time = record.EventHeader.TimeStamp as u64;
    let now = process::now();
    if now.saturating_sub(state.last_refresh) >= 10_000_000 {
        state.tracker.refresh();
        state.last_refresh = now;
    }
    if process_event {
        match state.decoder.process(record, opcode == 1) {
            Ok((pid, name)) => {
                if let Some(name) = name {
                    if let Err(error) = state.tracker.start(pid, time, name) {
                        state.warn(error);
                    }
                } else {
                    state.tracker.end(pid, time);
                }
                // Delayed starts/ends can only resolve this PID's queued generations.
                state.resolve_pid(pid, now);
            }
            Err(error) => state.warn(error),
        }
        return;
    }
    let (pid, size) = match state.decoder.network(record) {
        Ok(value) => value,
        Err(error) => {
            state.warn(error);
            return;
        }
    };
    state.decoded_total = state.decoded_total.saturating_add(1);
    let bytes = match direction {
        Some(Direction::Receive) => Bytes {
            received: size,
            sent: 0,
        },
        Some(Direction::Send) => Bytes {
            received: 0,
            sent: size,
        },
        None => return,
    };
    let event = NetworkEvent {
        pid,
        time,
        received: now,
        bytes,
    };
    if !state.account(event) {
        // A cache miss may be a new live process or an older delayed generation. Capture
        // validates birth/exit against event time, so a reused PID cannot substitute identity.
        state.tracker.retry_capture(pid, time);
        if !state.account(event) {
            state.defer(event);
        }
    }
}
