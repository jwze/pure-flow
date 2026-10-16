// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
#[path = "macos_native.rs"]
mod macos_native;
#[path = "macos_network.rs"]
mod macos_network;
use macos_native::{ProcessName, ProcessResolver};

use crate::CollectorError;
use pureflow_core::{Bytes, CounterMode, ProcessCounter, RawSample};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, ChildStderr, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const LINE_LIMIT: usize = 16 * 1024;
const PROCESS_LIMIT: usize = 16 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
const HEADER: &str = ",bytes_in,bytes_out,";

pub struct MacCollector {
    pending: Arc<Mutex<Accumulator>>,
    stop: Arc<AtomicBool>,
    wake: UnixStream,
    worker: Option<JoinHandle<()>>,
}
impl MacCollector {
    pub fn start() -> Result<Self, CollectorError> {
        // Local nettop(1): -d is delta mode, -P process summaries, -L 0 infinite
        // CSV logging, -s 1 one-second updates; -J restricts columns. Its column
        // order is implementation-defined, so the decoder validates it exactly.
        let (wake, stop_reader) = UnixStream::pair().map_err(unavailable)?;
        nonblocking(&wake)?;
        nonblocking(&stop_reader)?;
        let child = PipeCommand::spawn_stream(
            "/usr/bin/nettop",
            &[
                "-d",
                "-P",
                "-L",
                "0",
                "-s",
                "1",
                "-n",
                "-J",
                "bytes_in,bytes_out",
            ],
        )?;
        let pending = Arc::new(Mutex::new(Accumulator::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_pending = Arc::clone(&pending);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("pureflow-nettop".into())
            .spawn(move || {
                if let Err(error) =
                    collect_intervals(child, stop_reader, &worker_pending, &worker_stop)
                {
                    if let Ok(mut state) = worker_pending.lock() {
                        state.error = Some(error.to_string());
                    }
                }
            })
            .map_err(unavailable)?;
        Ok(Self {
            pending,
            stop,
            wake,
            worker: Some(worker),
        })
    }
    pub fn sample(&mut self) -> Result<RawSample, CollectorError> {
        let interfaces = macos_native::interfaces()?;
        if interfaces.is_empty() {
            return Err(unavailable(
                "NET_RT_IFLIST2 returned no physical en* interface counters",
            ));
        }
        let network = macos_network::read(&interfaces);
        let mut state = self
            .pending
            .lock()
            .map_err(|_| unavailable("nettop worker failed"))?;
        if let Some(error) = &state.error {
            return Err(unavailable(error));
        }
        let mut warnings = vec![
            "macOS process bytes are persistent nettop deltas, not lifetime counters; connections entirely between updates may be missed".into(),
            "nettop and libproc are separate snapshots; birth seconds/microseconds are revalidated around name lookup, but exits and PID reuse before lookup cannot be fully disambiguated".into(),
            "nettop batches are completed by the next header (about one second behind cumulative sysctl interface snapshots)".into(),
        ];
        warnings.extend(
            network
                .warnings
                .into_iter()
                .map(|warning| warning.replace("netstat", "sysctl")),
        );
        if !state.ready {
            warnings.push("nettop is warming up; its first absolute batch is discarded".into());
        }
        if state.unresolved > 0 {
            warnings.push(format!(
                "{} process counters omitted: birth identity unavailable",
                state.unresolved
            ));
        }
        if state.races > 0 {
            warnings.push(format!("{} process counters omitted: process exited or birth identity changed during libproc lookup", state.races));
        }
        if state.malformed > 0 {
            warnings.push(format!(
                "{} malformed nettop counter rows omitted",
                state.malformed
            ));
        }
        state.unresolved = 0;
        state.races = 0;
        state.malformed = 0;
        state.retained_size = 0;
        Ok(RawSample {
            interfaces,
            processes: state.processes.drain().map(|(_, value)| value).collect(),
            process_mode: CounterMode::Interval,
            network: network.network,
            warnings,
        })
    }
}
impl Drop for MacCollector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake poll immediately; EAGAIN means a notification is already queued.
        let _ = self.wake.write(&[1]);
        // The sole worker owns the child and reaps it before returning.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn unavailable(message: impl ToString) -> CollectorError {
    CollectorError::Unavailable(message.to_string())
}

fn nonblocking(pipe: &impl AsRawFd) -> Result<(), CollectorError> {
    // SAFETY: the pipe owns a live fd; F_GETFL has no third argument and
    // F_SETFL takes an integer flags argument. No pointer crosses this call.
    unsafe {
        let flags = libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL);
        if flags == -1
            || libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == -1
        {
            return Err(unavailable(io::Error::last_os_error()));
        }
    }
    Ok(())
}
fn output_terminal() -> Result<(File, File), CollectorError> {
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: openpty writes two fds into valid pointers. Optional name,
    // termios and winsize are null. Each successful fd gains exactly one owner.
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == -1 {
        return Err(unavailable(io::Error::last_os_error()));
    }
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // The GUI owns the master exclusively: closing it (including SIGKILL) must
    // hang up nettop's controlling terminal, not leak into another child.
    for file in [&master, &slave] {
        // SAFETY: both descriptors are live, and F_SETFD takes integer flags.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(unavailable(io::Error::last_os_error()));
        }
    }
    Ok((master, slave))
}
struct PipeCommand {
    child: Child,
    stdout: File,
    stderr: ChildStderr,
}
impl PipeCommand {
    fn spawn_stream(path: &str, arguments: &[&str]) -> Result<Self, CollectorError> {
        // nettop's infinite CSV logging is block buffered on a pipe (observed
        // no bytes for 15 seconds for an isolated PID). A PTY makes stdio flush
        // every row. -L explicitly keeps CSV logging even on a terminal.
        let (master, slave) = output_terminal()?;
        Self::spawn_with_output(path, arguments, master, slave)
    }
    fn spawn_with_output(
        path: &str,
        arguments: &[&str],
        master: File,
        slave: File,
    ) -> Result<Self, CollectorError> {
        let stdin = slave.try_clone().map_err(unavailable)?;
        let mut command = Command::new(path);
        command
            .args(arguments)
            .env("LC_ALL", "C")
            // stdin must stay a live PTY, not /dev/null: nettop can spin on
            // stdin EOF even in CSV mode. Both endpoints use the same slave.
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(slave))
            .stderr(Stdio::piped());
        // SAFETY: after fork only async-signal-safe libc operations are used;
        // stdin has been installed on fd 0 before pre_exec runs. No allocations
        // or locks occur on the success path. setsid detaches the GUI session.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| unavailable(format!("{path}: {e}")))?;
        let stdout = master;
        let stderr = child.stderr.take().expect("piped stderr");
        let command = Self {
            child,
            stdout,
            stderr,
        };
        nonblocking(&command.stdout)?;
        nonblocking(&command.stderr)?;
        Ok(command)
    }
}
impl Drop for PipeCommand {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Bound work per poll so a continuously writing pipe cannot starve stderr,
// timeout checks or shutdown. EOF is distinct from WouldBlock.
fn poll_pipe(
    pipe: &mut impl Read,
    mut consume: impl FnMut(&[u8]) -> Result<(), CollectorError>,
) -> Result<bool, CollectorError> {
    let mut buffer = [0u8; 8192];
    for _ in 0..8 {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => consume(&buffer[..count])?,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(unavailable(error)),
        }
    }
    Ok(false)
}

#[derive(Default)]
struct BatchDecoder {
    line: Vec<u8>,
    batch: Option<HashMap<u32, Bytes>>,
    batch_size: usize,
    malformed: u64,
    discarded_startup: bool,
}
struct Batch {
    counters: HashMap<u32, Bytes>,
    malformed: u64,
}
impl BatchDecoder {
    fn feed(
        &mut self,
        bytes: &[u8],
        mut complete: impl FnMut(Batch) -> Result<(), CollectorError>,
    ) -> Result<(), CollectorError> {
        for &byte in bytes {
            if byte != b'\n' {
                if self.line.len() == LINE_LIMIT {
                    return Err(unavailable("nettop row exceeds 16 KiB"));
                }
                self.line.push(byte);
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = std::str::from_utf8(&line)
                .map_err(|_| unavailable("nettop output is not UTF-8"))?
                .trim_end_matches('\r');
            if line == HEADER {
                if let Some(counters) = self.batch.take() {
                    if self.discarded_startup {
                        complete(Batch {
                            counters,
                            malformed: self.malformed,
                        })?;
                    } else {
                        // -d's first batch is startup absolute values, not an interval.
                        self.discarded_startup = true;
                    }
                }
                self.batch = Some(HashMap::new());
                self.batch_size = 0;
                self.malformed = 0;
            } else {
                // A header is never a process row. Reject changed/reordered or
                // extra columns instead of guessing which counter means what.
                if line.starts_with(',') || self.batch.is_none() {
                    return Err(unavailable("nettop returned an unsupported schema"));
                }
                self.batch_size += line.len() + 1;
                if self.batch_size > OUTPUT_LIMIT {
                    return Err(unavailable("nettop batch exceeds 8 MiB"));
                }
                let Some((pid, bytes)) = decode_row(line) else {
                    self.malformed = self.malformed.saturating_add(1);
                    continue;
                };
                let batch = self.batch.as_mut().expect("validated header");
                if batch.len() >= PROCESS_LIMIT {
                    return Err(unavailable("nettop batch exceeds process limit"));
                }
                if batch.insert(pid, bytes).is_some() {
                    return Err(unavailable("nettop returned duplicate process rows"));
                }
            }
        }
        Ok(())
    }
}
fn decode_row(line: &str) -> Option<(u32, Bytes)> {
    // nettop does not quote names containing commas. Exactly two validated
    // trailing counters and one trailing empty CSV field fix the schema.
    let row = line.strip_suffix(',')?;
    let mut fields = row.rsplitn(3, ',');
    let sent = fields.next()?;
    let received = fields.next()?;
    let process = fields.next()?;
    let (name, pid) = process.rsplit_once('.')?;
    if name.is_empty() || !decimal(pid) || !decimal(received) || !decimal(sent) {
        return None;
    }
    let pid = pid.parse().ok()?;
    if pid == 0 {
        return None;
    }
    Some((
        pid,
        Bytes {
            received: received.parse().ok()?,
            sent: sent.parse().ok()?,
        },
    ))
}
fn decimal(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Default)]
struct Accumulator {
    processes: HashMap<String, ProcessCounter>,
    retained_size: usize,
    unresolved: u64,
    races: u64,
    malformed: u64,
    ready: bool,
    error: Option<String>,
}
impl Accumulator {
    fn add(
        &mut self,
        batch: Batch,
        names: &HashMap<u32, ProcessName>,
    ) -> Result<(), CollectorError> {
        self.ready = true;
        self.malformed = self.malformed.saturating_add(batch.malformed);
        for (pid, bytes) in batch.counters {
            if bytes == Bytes::default() {
                continue;
            }
            let Some(mapped) = names.get(&pid) else {
                self.unresolved = self.unresolved.saturating_add(1);
                continue;
            };
            let identity = format!("{pid}:{}:{}", mapped.birth.sec, mapped.birth.usec);
            if !self.processes.contains_key(&identity) && self.processes.len() >= PROCESS_LIMIT {
                return Err(unavailable(
                    "nettop pending accumulator exceeds process limit; sample more frequently",
                ));
            }
            if !self.processes.contains_key(&identity) {
                let size =
                    identity.len() + mapped.name.len() + std::mem::size_of::<ProcessCounter>();
                if self.retained_size + size > OUTPUT_LIMIT {
                    return Err(unavailable(
                        "nettop pending accumulator exceeds 8 MiB; sample more frequently",
                    ));
                }
                self.retained_size += size;
            }
            let counter =
                self.processes
                    .entry(identity.clone())
                    .or_insert_with(|| ProcessCounter {
                        identity,
                        pid,
                        name: mapped.name.clone(),
                        bytes: Bytes::default(),
                    });
            if counter.name != mapped.name {
                return Err(unavailable(
                    "process name changed within the same birth identity",
                ));
            }
            counter.bytes = counter.bytes.checked_add(bytes).map_err(unavailable)?;
        }
        Ok(())
    }
}
fn collect_intervals(
    mut child: PipeCommand,
    stop_reader: UnixStream,
    pending: &Mutex<Accumulator>,
    stop: &AtomicBool,
) -> Result<(), CollectorError> {
    let mut decoder = BatchDecoder::default();
    let mut resolver = ProcessResolver::default();
    let mut last_batch = Instant::now();
    let mut stderr_open = true;
    let mut stderr_size = 0usize;
    while !stop.load(Ordering::Acquire) {
        let mut fds = [
            libc::pollfd {
                fd: child.stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if stderr_open {
                    child.stderr.as_raw_fd()
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // One kernel wait handles both outputs and explicit cancellation. The
        // one-second ceiling checks child exit/stall; it is not a busy loop.
        let timeout = COMMAND_TIMEOUT
            .saturating_sub(last_batch.elapsed())
            .min(Duration::from_secs(1))
            .as_millis() as i32;
        // SAFETY: the pollfd array stays writable/live for this synchronous call.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(unavailable(error));
        }
        if stop.load(Ordering::Acquire) || fds[2].revents != 0 {
            return Ok(());
        }
        if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
            return Err(unavailable("nettop poll descriptor became invalid"));
        }
        // Drain stderr first, bounded per wakeup and per interval, never include
        // potentially private subprocess text in diagnostics or retain it.
        if fds[1].revents != 0 {
            stderr_open = !poll_pipe(&mut child.stderr, |bytes| {
                stderr_size = stderr_size.saturating_add(bytes.len());
                if stderr_size > OUTPUT_LIMIT {
                    return Err(unavailable("nettop stderr exceeds 8 MiB per interval"));
                }
                Ok(())
            })?;
        }
        if fds[0].revents != 0 {
            let eof = poll_pipe(&mut child.stdout, |bytes| {
                decoder.feed(bytes, |batch| {
                    let mapped =
                        resolver.resolve_batch(batch.counters.iter().filter_map(|(pid, bytes)| {
                            (*bytes != Bytes::default()).then_some(*pid)
                        }));
                    let mut state = pending
                        .lock()
                        .map_err(|_| unavailable("nettop accumulator failed"))?;
                    state.races = state.races.saturating_add(mapped.races);
                    state.add(batch, &mapped.names)?;
                    last_batch = Instant::now();
                    stderr_size = 0;
                    Ok(())
                })
            })?;
            if eof {
                return Err(unavailable(
                    "persistent nettop exited; partial final batch discarded",
                ));
            }
        }
        if child.child.try_wait().map_err(unavailable)?.is_some() {
            return Err(unavailable(
                "persistent nettop exited; partial final batch discarded",
            ));
        }
        if last_batch.elapsed() >= COMMAND_TIMEOUT {
            return Err(unavailable(
                "persistent nettop produced no complete interval for 8 seconds",
            ));
        }
    }
    Ok(())
}
