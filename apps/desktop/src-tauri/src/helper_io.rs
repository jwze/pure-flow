// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! One outstanding operation per pipe. OVERLAPPED and its buffer remain alive
//! until completion, including cancellation; no detached I/O or polling reads.
use super::{os_error, Handle};
use std::{
    ptr::null_mut,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Storage::FileSystem::{ReadFile, WriteFile},
    System::{Pipes::ConnectNamedPipe, Threading::*, IO::*},
};

pub(super) struct Io<'a> {
    pub pipe: &'a Handle,
    pub peer: &'a Handle,
    pub cancel: Option<&'a AtomicBool>,
}
impl Io<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|v| v.load(Ordering::Acquire))
    }
    fn operation(
        &self,
        deadline: Option<Instant>,
        start: impl FnOnce(*mut OVERLAPPED) -> Result<bool, String>,
    ) -> Result<u32, String> {
        if self.cancelled() {
            return Err("IPC cancelled".into());
        }
        let event = unsafe { CreateEventW(null_mut(), 1, 0, null_mut()) };
        if event.is_null() {
            return Err(os_error("IPC completion event"));
        }
        let event = Handle(event);
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        if start(&mut overlapped)? {
            return Ok(0); // ConnectNamedPipe: already connected, no pending operation.
        }
        let handles = [event.0, self.peer.0];
        let failure = loop {
            if self.cancelled() {
                break "IPC cancelled".to_string();
            }
            let wait = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break "IPC timeout".to_string();
                    }
                    remaining.as_millis().clamp(1, 100) as u32
                }
                None if self.cancel.is_none() => INFINITE,
                None => 100,
            };
            match unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, wait) } {
                WAIT_OBJECT_0 => {
                    let mut count = 0;
                    if unsafe { GetOverlappedResult(self.pipe.0, &overlapped, &mut count, 0) } == 0
                    {
                        return Err(os_error("IPC completion"));
                    }
                    return Ok(count);
                }
                v if v == WAIT_OBJECT_0 + 1 => break "IPC peer exited".to_string(),
                WAIT_TIMEOUT => continue,
                _ => break os_error("IPC wait"),
            }
        };
        // Cancel ONLY this operation. Even ERROR_NOT_FOUND can mean completion
        // raced cancellation. Drain before releasing OVERLAPPED/buffer/event.
        // Named-pipe cancellation completes in the kernel; never detach a worker
        // or free its buffer while Windows may still access it.
        unsafe {
            CancelIoEx(self.pipe.0, &overlapped);
            let mut count = 0;
            GetOverlappedResult(self.pipe.0, &overlapped, &mut count, 1);
        }
        Err(failure)
    }
    pub fn connect(&self, timeout: Duration) -> Result<(), String> {
        self.operation(Some(Instant::now() + timeout), |overlapped| {
            if unsafe { ConnectNamedPipe(self.pipe.0, overlapped) } != 0 {
                return Ok(false);
            }
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => Ok(true),
                ERROR_IO_PENDING => Ok(false),
                _ => Err(os_error("ConnectNamedPipe")),
            }
        })
        .map(|_| ())
    }
    pub fn write(&self, bytes: &[u8], deadline: Instant) -> Result<(), String> {
        let mut offset = 0;
        while offset < bytes.len() {
            let count = self.operation(Some(deadline), |overlapped| {
                let ok = unsafe {
                    WriteFile(
                        self.pipe.0,
                        bytes[offset..].as_ptr(),
                        (bytes.len() - offset) as u32,
                        null_mut(),
                        overlapped,
                    )
                };
                if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                    return Err(os_error("Pipe write"));
                }
                Ok(false)
            })? as usize;
            if count == 0 {
                return Err("IPC write made no progress".into());
            }
            offset += count;
        }
        Ok(())
    }
    pub fn read(&self, bytes: &mut [u8], deadline: Option<Instant>) -> Result<(), String> {
        let mut offset = 0;
        // Idle helper header waits indefinitely on completion OR GUI death. Once
        // any bytes arrive, a partial frame cannot hold the helper indefinitely.
        let mut deadline = deadline;
        while offset < bytes.len() {
            let count = self.operation(deadline, |overlapped| {
                let ok = unsafe {
                    ReadFile(
                        self.pipe.0,
                        bytes[offset..].as_mut_ptr(),
                        (bytes.len() - offset) as u32,
                        null_mut(),
                        overlapped,
                    )
                };
                if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                    return Err(os_error("Pipe read"));
                }
                Ok(false)
            })? as usize;
            if count == 0 {
                return Err("IPC peer disconnected".into());
            }
            offset += count;
            deadline.get_or_insert_with(|| Instant::now() + super::TIMEOUT);
        }
        Ok(())
    }
}
