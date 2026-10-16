// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Acquisition precedes database/collector setup. Never terminate another process.
#[cfg(target_os = "macos")]
mod mac {
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::{
            fd::AsRawFd,
            unix::{
                fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
                net::{UnixListener, UnixStream},
            },
        },
        path::PathBuf,
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    pub struct Singleton {
        _lock: File,
        listener: Option<UnixListener>,
        socket: PathBuf,
        wake: Option<UnixStream>,
        worker: Option<JoinHandle<()>>,
    }
    impl Singleton {
        pub fn claim() -> Result<Option<Self>, String> {
            // A short per-UID path avoids sockaddr_un's path-length limit.
            let uid = unsafe { libc::geteuid() };
            let root = PathBuf::from(format!("/tmp/pureflow-{uid}"));
            match fs::create_dir(&root) {
                Ok(()) => fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                    .map_err(|e| e.to_string())?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.to_string()),
            }
            let meta = fs::symlink_metadata(&root).map_err(|e| e.to_string())?;
            if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
                return Err("不安全的 PureFlow 运行目录，已拒绝启动".into());
            }
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(root.join("owner.lock"))
                .map_err(|e| e.to_string())?;
            let meta = lock.metadata().map_err(|e| e.to_string())?;
            if !meta.is_file() || meta.uid() != uid || meta.nlink() != 1 || meta.mode() & 0o077 != 0
            {
                return Err("不安全的 PureFlow 所有权文件".into());
            }
            let socket = root.join("notify.sock");
            // SAFETY: flock operates on the live owned descriptor, no pointers.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                    return Err(error.to_string());
                }
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    if let Ok(mut stream) = UnixStream::connect(&socket) {
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                        let _ = stream.write_all(b"S");
                        break;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                return Ok(None);
            }
            // Only the lock holder can reclaim a stale notification socket.
            match fs::remove_file(&socket) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
            let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            Ok(Some(Self {
                _lock: lock,
                listener: Some(listener),
                socket,
                wake: None,
                worker: None,
            }))
        }
        pub fn attach(&mut self, app: tauri::AppHandle) -> Result<(), String> {
            let listener = self.listener.take().ok_or("单实例通知已启动")?;
            let (wake, receiver) = UnixStream::pair().map_err(|e| e.to_string())?;
            self.worker = Some(
                thread::Builder::new()
                    .name("pureflow-singleton".into())
                    .spawn(move || {
                        let mut fds = [
                            libc::pollfd {
                                fd: listener.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                            libc::pollfd {
                                fd: receiver.as_raw_fd(),
                                events: libc::POLLIN,
                                revents: 0,
                            },
                        ];
                        loop {
                            // SAFETY: two initialized pollfd entries outlive the blocking call.
                            let result =
                                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
                            if result < 0 {
                                if std::io::Error::last_os_error().kind()
                                    == std::io::ErrorKind::Interrupted
                                {
                                    continue;
                                }
                                break;
                            }
                            if fds[1].revents != 0 {
                                break;
                            }
                            if fds[0].revents & libc::POLLIN != 0 {
                                // Bound per wake so client floods cannot starve shutdown.
                                for _ in 0..8 {
                                    let Ok((mut stream, _)) = listener.accept() else {
                                        break;
                                    };
                                    let _ =
                                        stream.set_read_timeout(Some(Duration::from_millis(100)));
                                    let mut command = [0];
                                    if stream.read_exact(&mut command).is_ok() && command == *b"S" {
                                        let _ = crate::shell::show_panel(&app);
                                    }
                                }
                            }
                        }
                    })
                    .map_err(|e| e.to_string())?,
            );
            self.wake = Some(wake);
            Ok(())
        }
    }
    impl Drop for Singleton {
        fn drop(&mut self) {
            if let Some(mut wake) = self.wake.take() {
                let _ = wake.write_all(b"X");
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            let _ = fs::remove_file(&self.socket);
            // Stable lock inode deliberately remains; close releases ownership atomically.
        }
    }
}
#[cfg(target_os = "macos")]
pub use mac::Singleton;

#[cfg(windows)]
pub struct CollectorOwnership(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for CollectorOwnership {}
#[cfg(windows)]
unsafe impl Sync for CollectorOwnership {}
#[cfg(windows)]
impl CollectorOwnership {
    pub fn claim() -> Result<Self, String> {
        use windows_sys::Win32::{Foundation::*, System::Threading::*};
        let name: Vec<u16> = "Local\\xyz.115613.pureflow.collector-owner"
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: named mutex only coordinates this ordinary GUI; no other process is killed.
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() {
                return Err(format!("采集所有权锁失败：{}", GetLastError()));
            }
            let result = WaitForSingleObject(handle, 0);
            if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
                CloseHandle(handle);
                return Err("已有净流实例持有采集权限，已拒绝重复采集".into());
            }
            Ok(Self(handle))
        }
    }
}
#[cfg(windows)]
impl Drop for CollectorOwnership {
    fn drop(&mut self) {
        // Process exit also releases the mutex, including abnormal termination.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
