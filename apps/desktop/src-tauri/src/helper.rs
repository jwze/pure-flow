// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Fixed-operation privileged collector. Local byte-mode named pipe, current-user
//! and SYSTEM DACL, random name, peer PID checks, bounded frames and bounded waits.
//! This is not a shell/service. The GUI never executes user-selected elevated code.
use pureflow_core::RawSample;
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    mem::size_of,
    path::PathBuf,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, Cryptography::*, *},
    Storage::FileSystem::*,
    System::{Pipes::*, Threading::*},
    UI::{Shell::*, WindowsAndMessaging::*},
};
#[path = "helper_io.rs"]
mod io;
#[path = "helper_owner.rs"]
mod owner;
const MAX_FRAME: usize = 8 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
const PREFIX: &str = "\\\\.\\pipe\\PureFlow-";
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
enum Command {
    Hello { nonce: String },
    Sample,
    Pause { paused: bool },
    Stop,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    sample: Option<RawSample>,
    error: Option<String>,
    /// Terminal reply: emitted only after cleanup was attempted, never a stale
    /// Sample/Hello acknowledgment that could be mistaken for Stop completion.
    stopped: bool,
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
fn os_error(label: &str) -> String {
    format!("{label}: Windows error {}", unsafe { GetLastError() })
}
struct Handle(HANDLE);
// Handles are owned by the worker and never used concurrently; Win32 handles
// may be transferred between threads. They are intentionally not Sync.
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn token() -> Result<Handle, String> {
    let mut h = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h) } == 0 {
        return Err(os_error("OpenProcessToken"));
    }
    Ok(Handle(h))
}
pub fn ensure_gui_not_elevated() -> Result<(), String> {
    let t = token()?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut n = 0;
    if unsafe {
        GetTokenInformation(
            t.0,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        )
    } == 0
    {
        return Err(os_error("TokenElevation"));
    }
    if elevation.TokenIsElevated != 0 {
        Err("请以普通权限启动净流；仅独立采集器需要管理员权限，不允许提权 WebView。".into())
    } else {
        Ok(())
    }
}
pub fn show_startup_error(message: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(message).as_ptr(),
            wide("净流 PureFlow").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
fn nonce() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    if unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    } < 0
    {
        return Err("Cannot generate IPC nonce".into());
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn valid_nonce(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn pipe_name(pid: u32, nonce: &str) -> String {
    format!("{PREFIX}{pid}-{nonce}")
}
fn pipe_server(name: &str) -> Result<Handle, String> {
    let t = token()?;
    let mut size = 0;
    unsafe {
        GetTokenInformation(t.0, TokenUser, null_mut(), 0, &mut size);
    }
    if size == 0 || size > 65536 {
        return Err("Invalid token user size".into());
    }
    let mut storage = vec![0u64; (size as usize).div_ceil(8)];
    if unsafe { GetTokenInformation(t.0, TokenUser, storage.as_mut_ptr().cast(), size, &mut size) }
        == 0
    {
        return Err(os_error("TokenUser"));
    }
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(os_error("ConvertSid"));
    }
    let sid_memory = LocalMemory(sid.cast());
    let mut len = 0;
    unsafe {
        while *sid.add(len) != 0 {
            len += 1;
        }
    }
    let sid_text = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid, len)) };
    drop(sid_memory);
    // Protected ACL: only the launching user's SID and SYSTEM. No Everyone,
    // network or anonymous access. PIPE_REJECT_REMOTE_CLIENTS is additional.
    let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid_text})"));
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(os_error("Pipe DACL"));
    }
    let memory = LocalMemory(descriptor);
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let h = unsafe {
        CreateNamedPipeW(
            wide(name).as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            0,
            &security,
        )
    };
    drop(memory);
    if h == INVALID_HANDLE_VALUE {
        Err(os_error("CreateNamedPipe"))
    } else {
        Ok(Handle(h))
    }
}
fn process_alive(process: &Handle) -> bool {
    unsafe { WaitForSingleObject(process.0, 0) == WAIT_TIMEOUT }
}
fn frame_size(size: u32) -> Result<usize, String> {
    let n = size as usize;
    if n == 0 || n > MAX_FRAME {
        Err("IPC frame exceeds safety limit".into())
    } else {
        Ok(n)
    }
}
fn send<T: Serialize>(io: &io::Io<'_>, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err("IPC frame exceeds safety limit".into());
    }
    let deadline = Instant::now() + TIMEOUT;
    io.write(&(bytes.len() as u32).to_le_bytes(), deadline)?;
    io.write(&bytes, deadline)
}
fn receive<T: serde::de::DeserializeOwned>(io: &io::Io<'_>, idle: bool) -> Result<T, String> {
    let mut header = [0; 4];
    io.read(&mut header, (!idle).then(|| Instant::now() + TIMEOUT))?;
    let n = frame_size(u32::from_le_bytes(header))?;
    let mut bytes = vec![0; n];
    io.read(&mut bytes, Some(Instant::now() + TIMEOUT))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid IPC message: {e}"))
}
fn verify_helper_bytes(bytes: &[u8], expected: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    if expected.len() != 64 || !expected.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("Invalid embedded collector digest".into());
    }
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected.to_ascii_lowercase() {
        return Err("独立采集器完整性校验失败，已拒绝请求 UAC；请从可信来源重新安装完整包".into());
    }
    Ok(())
}
fn verify_helper_file(path: &std::path::Path) -> Result<(), String> {
    match option_env!("PUREFLOW_HELPER_SHA256") {
        Some(expected) => {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            verify_helper_bytes(&bytes, expected)
        }
        None if cfg!(debug_assertions) => Ok(()), // Local developer builds only.
        None => Err("Release GUI lacks embedded collector identity; build with the desktop packaging command".into()),
    }
}
fn helper_executable() -> Result<PathBuf, String> {
    let dir = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .parent()
        .ok_or("GUI directory missing")?
        .to_path_buf();
    for path in [
        dir.join("pureflow-collector.exe"),
        dir.join("helper/pureflow-collector.exe"),
    ] {
        if path.is_file() {
            let path = path.canonicalize().map_err(|e| e.to_string())?;
            verify_helper_file(&path)?;
            return Ok(path);
        }
    }
    Err("独立采集器未打包，请重新安装完整安装包或执行桌面构建脚本".into())
}
fn launch(path: &std::path::Path, parent: u32, nonce: &str) -> Result<Handle, String> {
    let file = wide(&path.to_string_lossy());
    let parameters = wide(&format!("--parent {parent} --nonce {nonce}"));
    let verb = wide("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_HIDE,
        ..Default::default()
    };
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let e = unsafe { GetLastError() };
        return Err(if e == ERROR_CANCELLED {
            "已取消采集器 UAC 授权；点击重新采集可再次请求".into()
        } else {
            format!("启动独立采集器失败：{e}")
        });
    }
    if info.hProcess.is_null() {
        return Err("采集器进程句柄不可用".into());
    }
    Ok(Handle(info.hProcess))
}
pub struct HelperCollector {
    pipe: Handle,
    process: Handle,
    cancel: Arc<AtomicBool>,
    closed: bool,
    shutdown_error: Option<String>,
    terminal_reply: bool,
}
impl HelperCollector {
    /// Cancellation is irreversible for this collector: set true to interrupt
    /// in-flight IPC within 100ms, then drop/join it. Pause/resume must not set
    /// this token because the already-authorized helper remains resident.
    pub fn start_with_cancel(cancel: Arc<AtomicBool>) -> Result<Self, String> {
        if cancel.load(Ordering::Acquire) {
            return Err("Collector start cancelled".into());
        }
        ensure_gui_not_elevated()?;
        owner::preflight()?;
        let parent = unsafe { GetCurrentProcessId() };
        let n = nonce()?;
        let pipe = pipe_server(&pipe_name(parent, &n))?;
        let process = launch(&helper_executable()?, parent, &n)?;
        let mut collector = Self {
            pipe,
            process,
            cancel,
            closed: false,
            shutdown_error: None,
            terminal_reply: false,
        };
        let startup = (|| {
            collector.io().connect(TIMEOUT)?;
            let mut peer = 0;
            if unsafe { GetNamedPipeClientProcessId(collector.pipe.0, &mut peer) } == 0
                || peer != unsafe { GetProcessId(collector.process.0) }
            {
                return Err("IPC client process identity mismatch".into());
            }
            collector.request(Command::Hello { nonce: n })?;
            Ok(())
        })();
        if let Err(error) = startup {
            return Err(combine(error, collector.shutdown()));
        }
        Ok(collector)
    }
    fn io(&self) -> io::Io<'_> {
        io::Io {
            pipe: &self.pipe,
            peer: &self.process,
            cancel: Some(&self.cancel),
        }
    }
    fn request(&mut self, command: Command) -> Result<Reply, String> {
        if self.closed {
            return Err(
                "Collector connection is closed; click retry to request a new helper".into(),
            );
        }
        let io = self.io();
        send(&io, &command)?;
        let reply: Reply = receive(&io, false)?;
        self.terminal_reply = reply.stopped;
        if let Some(error) = &reply.error {
            return Err(error.clone());
        }
        Ok(reply)
    }
    pub fn sample(&mut self) -> Result<RawSample, String> {
        self.request(Command::Sample)?
            .sample
            .ok_or("Collector returned no sample".into())
    }
    pub fn pause(&mut self, paused: bool) -> Result<(), String> {
        self.request(Command::Pause { paused }).map(|_| ())
    }
    /// Explicit callers receive both helper cleanup acknowledgments and process
    /// exit failures. Drop is only the reporting fallback, never a success claim.
    pub fn shutdown(&mut self) -> Result<(), String> {
        if self.closed {
            return self.shutdown_error.clone().map_or(Ok(()), Err);
        }
        self.closed = true;
        let mut failure = None;
        if process_alive(&self.process) && !self.terminal_reply {
            let io = io::Io {
                pipe: &self.pipe,
                peer: &self.process,
                cancel: None,
            };
            // Bound shutdown independently of the ordinary request timeout.
            // The helper acknowledges Stop only after fallible ETW cleanup.
            let exchange = (|| {
                let bytes = serde_json::to_vec(&Command::Stop).map_err(|e| e.to_string())?;
                let deadline = Instant::now() + Duration::from_millis(200);
                io.write(&(bytes.len() as u32).to_le_bytes(), deadline)?;
                io.write(&bytes, deadline)?;
                let mut header = [0; 4];
                io.read(&mut header, Some(Instant::now() + Duration::from_secs(2)))?;
                let mut bytes = vec![0; frame_size(u32::from_le_bytes(header))?];
                io.read(
                    &mut bytes,
                    Some(Instant::now() + Duration::from_millis(500)),
                )?;
                let reply: Reply = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                if !reply.stopped {
                    return Err("Received non-terminal reply instead of Stop acknowledgment".into());
                }
                reply.error.map_or(Ok(()), Err)
            })();
            if let Err(error) = exchange {
                failure = Some(format!("Collector shutdown acknowledgment failed: {error}"));
                if unsafe { DisconnectNamedPipe(self.pipe.0) } == 0 {
                    let error = unsafe { GetLastError() };
                    if error != ERROR_PIPE_NOT_CONNECTED {
                        failure = Some(combine(
                            failure.take().unwrap(),
                            Err(format!("DisconnectNamedPipe: {error}")),
                        ));
                    }
                }
            }
        }
        match unsafe { WaitForSingleObject(self.process.0, 2000) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
                    let error = os_error("Collector exit status");
                    failure = Some(failure.map_or(error.clone(), |old| combine(old, Err(error))));
                } else if code != 0 {
                    let error =
                        format!("Collector exited with status {code}; cleanup was not confirmed");
                    failure = Some(failure.map_or(error.clone(), |old| combine(old, Err(error))));
                }
            }
            result => {
                let error = format!("Collector exit not confirmed (wait {result}); ownership remains reserved while it is alive; no replacement will be started automatically");
                failure = Some(failure.map_or(error.clone(), |old| combine(old, Err(error))));
            }
        }
        self.shutdown_error = failure.clone();
        failure.map_or(Ok(()), Err)
    }
}
impl Drop for HelperCollector {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("PureFlow collector cleanup: {error}");
        }
    }
}
fn combine(error: String, cleanup: Result<(), String>) -> String {
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
    }
}
fn parent_process(pid: u32) -> Result<Handle, String> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if h.is_null() {
        return Err(os_error("Parent process"));
    }
    let parent = Handle(h);
    let mut name = vec![0u16; 32768];
    let mut len = name.len() as u32;
    if unsafe { QueryFullProcessImageNameW(parent.0, 0, name.as_mut_ptr(), &mut len) } == 0 {
        return Err(os_error("Parent executable"));
    }
    let path = PathBuf::from(String::from_utf16_lossy(&name[..len as usize]));
    let own = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let expected = if own
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|p| p == "helper")
    {
        own.parent()
            .and_then(|p| p.parent())
            .ok_or("helper directory")?
            .join("pureflow.exe")
    } else {
        own.parent().ok_or("helper directory")?.join("pureflow.exe")
    };
    if path.canonicalize().map_err(|e| e.to_string())?
        != expected.canonicalize().map_err(|e| e.to_string())?
    {
        return Err("Unexpected GUI parent executable".into());
    }
    // Replacing files in a user-writable install remains a signing/deployment
    // risk; this handshake does not claim to authenticate a signed publisher.
    Ok(parent)
}
pub fn run_helper(args: impl Iterator<Item = String>) -> Result<(), String> {
    let values: Vec<_> = args.collect();
    if values.len() != 4
        || values[0] != "--parent"
        || values[2] != "--nonce"
        || !valid_nonce(&values[3])
    {
        return Err("Invalid collector arguments".into());
    }
    let pid: u32 = values[1].parse().map_err(|_| "Invalid parent PID")?;
    if pid == 0 {
        return Err("Invalid parent PID".into());
    }
    let parent = parent_process(pid)?;
    let name = wide(&pipe_name(pid, &values[3]));
    let deadline = Instant::now() + TIMEOUT;
    let pipe = loop {
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            break Handle(h);
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_ACCESS_DENIED {
            return Err("Named pipe access denied: only same-account UAC elevation is supported; another administrator account cannot access this user's pipe".into());
        }
        if error != ERROR_FILE_NOT_FOUND && error != ERROR_PIPE_BUSY {
            return Err(format!("Collector pipe connection failed: {error}"));
        }
        if !process_alive(&parent) || Instant::now() >= deadline {
            return Err(os_error("Collector pipe connection"));
        }
        // Only connection retry (not steady-state I/O); wait also detects GUI death.
        if unsafe { WaitForSingleObject(parent.0, 100) } != WAIT_TIMEOUT {
            return Err("GUI exited before pipe connection".into());
        }
    };
    let mode = PIPE_READMODE_BYTE | PIPE_WAIT;
    if unsafe { SetNamedPipeHandleState(pipe.0, &mode, null(), null()) } == 0 {
        return Err(os_error("Pipe byte mode"));
    }
    let mut server = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server) } == 0 || server != pid {
        return Err("IPC server process mismatch".into());
    }
    let io = io::Io {
        pipe: &pipe,
        peer: &parent,
        cancel: None,
    };
    match receive::<Command>(&io, false)? {
        Command::Hello { nonce } if nonce == values[3] => {}
        _ => return Err("IPC handshake mismatch".into()),
    }
    // Acquire before ANY ETW startup, and retain outside the optional collector:
    // pause must not authorize a second helper to take over this login session.
    let _lease = match owner::Lease::acquire(&parent) {
        Ok(lease) => lease,
        Err(error) => return Err(report_error(&io, error)),
    };
    if !process_alive(&parent) {
        return Err("GUI exited before collector startup".into());
    }
    let mut collector = match pureflow_collector::NativeCollector::start() {
        Ok(c) => Some(c),
        Err(error) => return Err(report_error(&io, error.to_string())),
    };
    if let Err(error) = send(
        &io,
        &Reply {
            sample: None,
            error: None,
            stopped: false,
        },
    ) {
        return Err(combine(error, stop_collector(&mut collector)));
    }
    let (outcome, acknowledge) = loop {
        let command: Command = match receive(&io, true) {
            Ok(command) => command,
            Err(error) => break (Err(error), false),
        };
        let result = match command {
            Command::Stop => break (Ok(()), true),
            Command::Hello { .. } => break (Err("Repeated handshake rejected".into()), true),
            Command::Sample => match collector.as_mut() {
                Some(c) => c.sample().map(Some).map_err(|e| e.to_string()),
                None => Err("Collector paused".into()),
            },
            Command::Pause { paused } => {
                if paused {
                    stop_collector(&mut collector).map(|()| None)
                } else if !process_alive(&parent) {
                    Err("GUI exited before collector resume".into())
                } else if collector.is_none() {
                    match pureflow_collector::NativeCollector::start() {
                        Ok(c) => {
                            collector = Some(c);
                            Ok(None)
                        }
                        Err(error) => Err(error.to_string()),
                    }
                } else {
                    Ok(None)
                }
            }
        };
        let sample = match result {
            Ok(sample) => sample,
            // Sample, resume and pause cleanup errors are terminal. Never leave
            // a failing collector active while the GUI believes it was stopped.
            Err(error) => break (Err(error), true),
        };
        if let Err(error) = send(
            &io,
            &Reply {
                sample,
                error: None,
                stopped: false,
            },
        ) {
            break (Err(error), false);
        }
    };
    let cleanup = stop_collector(&mut collector);
    let outcome = match outcome {
        Ok(()) => cleanup,
        Err(error) => Err(combine(error, cleanup)),
    };
    if acknowledge {
        let reply = Reply {
            sample: None,
            error: outcome.as_ref().err().cloned(),
            stopped: true,
        };
        if let Err(error) = send(&io, &reply) {
            return Err(combine(error, outcome));
        }
    }
    // Explicit cleanup precedes the Stop acknowledgment and lease release. On
    // failure the helper exits nonzero and the protected exact journal survives.
    outcome
}
fn stop_collector(
    collector: &mut Option<pureflow_collector::NativeCollector>,
) -> Result<(), String> {
    if let Some(active) = collector.as_mut() {
        active.shutdown().map_err(|error| error.to_string())?;
    }
    *collector = None;
    Ok(())
}
fn report_error(io: &io::Io<'_>, error: String) -> String {
    let delivery = send(
        io,
        &Reply {
            sample: None,
            error: Some(error.clone()),
            stopped: true,
        },
    );
    combine(error, delivery)
}
