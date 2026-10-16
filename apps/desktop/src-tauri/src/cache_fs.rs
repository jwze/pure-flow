// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Bounded, no-follow operations for fixed application cache roots selected by
//! storage_paths. These functions must never be exposed to arbitrary IPC paths.
//! Errors may leave a partially cleared cache; no recursive path-based removal
//! is used. Platform locks/descriptors stay alive throughout each traversal.
use std::path::Path;

#[cfg(any(target_os = "macos", windows))]
const MAX_ENTRIES: usize = 10_000;
#[cfg(any(target_os = "macos", windows))]
const MAX_DEPTH: usize = 32;

pub fn directory_bytes(path: &Path) -> Result<u64, String> {
    platform::run(path, false, false)
}

pub fn clear_directory(path: &Path, remove_root: bool) -> Result<(), String> {
    platform::run(path, true, remove_root).map(|_| ())
}

#[cfg(any(target_os = "macos", windows))]
fn consume(budget: &mut usize, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("缓存目录层级超出安全上限".into());
    }
    if *budget == 0 {
        return Err("缓存文件数量超出安全上限".into());
    }
    *budget -= 1;
    Ok(())
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        io,
        mem::MaybeUninit,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };

    fn error() -> String {
        io::Error::last_os_error().to_string()
    }

    fn name(bytes: &[u8]) -> Result<CString, String> {
        CString::new(bytes).map_err(|_| "缓存路径包含 NUL".into())
    }

    fn open_dir(parent: i32, name: &CStr) -> Result<OwnedFd, io::Error> {
        // SAFETY: name is NUL terminated; a successful fd is immediately owned.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: openat returned a new valid descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    fn stat_fd(fd: i32) -> Result<libc::stat, String> {
        let mut stat = MaybeUninit::uninit();
        // SAFETY: fstat initializes stat on success.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn stat_at(fd: i32, name: &CStr) -> Result<libc::stat, String> {
        let mut stat = MaybeUninit::uninit();
        // SAFETY: name is NUL terminated, output is valid, and links aren't followed.
        if unsafe {
            libc::fstatat(
                fd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(error());
        }
        Ok(unsafe { stat.assume_init() })
    }

    fn same(a: &libc::stat, b: &libc::stat) -> bool {
        a.st_dev == b.st_dev
            && a.st_ino == b.st_ino
            && a.st_mode & libc::S_IFMT == b.st_mode & libc::S_IFMT
    }

    fn validate(stat: &libc::stat, device: libc::dev_t) -> Result<bool, String> {
        if stat.st_dev != device {
            return Err("拒绝遍历缓存中的挂载点".into());
        }
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Ok(true),
            libc::S_IFREG if stat.st_nlink == 1 => Ok(false),
            libc::S_IFREG => Err("缓存含硬链接，拒绝处理".into()),
            _ => Err("缓存含符号链接或非常规文件，拒绝处理".into()),
        }
    }

    struct Entries(*mut libc::DIR);
    impl Entries {
        fn new(fd: i32) -> Result<Self, String> {
            // Duplicate with CLOEXEC atomically; fdopendir owns only the duplicate.
            // SAFETY: fcntl does not consume the pinned directory descriptor.
            let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if copy < 0 {
                return Err(error());
            }
            // SAFETY: copy is a valid directory fd; transferred on success only.
            let dir = unsafe { libc::fdopendir(copy) };
            if dir.is_null() {
                let err = error();
                // SAFETY: failed fdopendir did not take ownership.
                unsafe { libc::close(copy) };
                return Err(err);
            }
            Ok(Self(dir))
        }

        fn next(&mut self) -> Result<Option<CString>, String> {
            loop {
                // SAFETY: __error is this thread's errno; DIR is live and exclusively used.
                let entry = unsafe {
                    *libc::__error() = 0;
                    libc::readdir(self.0)
                };
                if entry.is_null() {
                    // SAFETY: read this thread's errno immediately after readdir.
                    return if unsafe { *libc::__error() } == 0 {
                        Ok(None)
                    } else {
                        Err(error())
                    };
                }
                // SAFETY: readdir returned a valid entry with a NUL-terminated d_name.
                let item = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                if item.to_bytes() != b"." && item.to_bytes() != b".." {
                    return Ok(Some(item.to_owned()));
                }
            }
        }
    }
    impl Drop for Entries {
        fn drop(&mut self) {
            // SAFETY: this owns the DIR and its duplicate fd exactly once.
            unsafe { libc::closedir(self.0) };
        }
    }

    fn unlink(
        parent: i32,
        name: &CStr,
        expected: &libc::stat,
        directory: bool,
    ) -> Result<(), String> {
        // Never remove a name known to refer to a replacement. A change after this
        // check can only unlink a name in this pinned parent, never follow a link.
        let current = stat_at(parent, name)?;
        if !same(&current, expected) {
            return Err("缓存条目在处理期间被替换".into());
        }
        validate(&current, expected.st_dev)?;
        // SAFETY: parent remains pinned; unlinkat never follows the final component.
        if unsafe {
            libc::unlinkat(
                parent,
                name.as_ptr(),
                if directory { libc::AT_REMOVEDIR } else { 0 },
            )
        } != 0
        {
            return Err(error());
        }
        Ok(())
    }

    fn walk(
        fd: i32,
        device: libc::dev_t,
        clear: bool,
        budget: &mut usize,
        depth: usize,
    ) -> Result<u64, String> {
        if depth > MAX_DEPTH {
            return Err("缓存目录层级超出安全上限".into());
        }
        let mut entries = Entries::new(fd)?;
        let mut total = 0u64;
        while let Some(name) = entries.next()? {
            consume(budget, depth)?;
            let stat = stat_at(fd, &name)?;
            let directory = validate(&stat, device)?;
            let bytes = if directory {
                let child = open_dir(fd, &name).map_err(|e| e.to_string())?;
                let opened = stat_fd(child.as_raw_fd())?;
                if !same(&stat, &opened) {
                    return Err("缓存目录在打开期间被替换".into());
                }
                let size = walk(child.as_raw_fd(), device, clear, budget, depth + 1)?;
                if clear {
                    unlink(fd, &name, &opened, true)?;
                }
                size
            } else {
                let size = u64::try_from(stat.st_size).map_err(|_| "缓存文件大小无效")?;
                if clear {
                    unlink(fd, &name, &stat, false)?;
                }
                size
            };
            total = total.checked_add(bytes).ok_or("缓存大小溢出")?;
        }
        Ok(total)
    }

    pub(super) fn run(path: &Path, clear: bool, remove_root: bool) -> Result<u64, String> {
        if !path.is_absolute() {
            return Err("缓存路径必须为绝对路径".into());
        }
        // Pin every component, never canonicalize (which would follow symlinks).
        let mut names = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir => (),
                Component::Normal(part) => names.push(name(part.as_bytes())?),
                _ => return Err("缓存路径含不安全分量".into()),
            }
        }
        if names.is_empty() {
            return Err("拒绝处理文件系统根目录".into());
        }
        let mut pins = vec![open_dir(libc::AT_FDCWD, &name(b"/")?).map_err(|e| e.to_string())?];
        for part in &names {
            let parent = pins.last().ok_or("缓存父目录缺失")?.as_raw_fd();
            match open_dir(parent, part) {
                Ok(fd) => pins.push(fd),
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(0),
                Err(e) => return Err(e.to_string()),
            }
        }
        let root = pins.last().ok_or("缓存目录缺失")?.as_raw_fd();
        let stat = stat_fd(root)?;
        let parent = pins[pins.len() - 2].as_raw_fd();
        if stat.st_dev != stat_fd(parent)?.st_dev {
            return Err("拒绝处理作为挂载点的缓存根目录".into());
        }
        let mut budget = MAX_ENTRIES;
        let total = walk(root, stat.st_dev, clear, &mut budget, 0)?;
        if clear && remove_root {
            unlink(parent, names.last().ok_or("缓存目录名称缺失")?, &stat, true)?;
        }
        Ok(total)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        fs, io,
        mem::{size_of, MaybeUninit},
        os::windows::ffi::OsStrExt,
        path::{Component, PathBuf, Prefix},
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{
            CreateFileW, FileDispositionInfo, GetFileInformationByHandle,
            SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ, OPEN_EXISTING,
        },
    };

    // DELETE standard access right. Keep sharing restrictive: a writable handle
    // could turn a pinned directory into a reparse point without renaming it.
    const DELETE_ACCESS: u32 = 0x0001_0000;
    struct Pin {
        handle: HANDLE,
        info: BY_HANDLE_FILE_INFORMATION,
    }
    impl Drop for Pin {
        fn drop(&mut self) {
            // SAFETY: Pin uniquely owns this valid handle.
            unsafe { CloseHandle(self.handle) };
        }
    }
    impl Pin {
        fn open(path: &Path, directory: bool, delete: bool) -> Result<Self, io::Error> {
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            if wide.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "缓存路径包含 NUL",
                ));
            }
            wide.push(0);
            let access = FILE_READ_ATTRIBUTES
                | if directory { FILE_LIST_DIRECTORY } else { 0 }
                | if delete { DELETE_ACCESS } else { 0 };
            // SAFETY: path is NUL terminated and the output handle is immediately owned.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    access,
                    FILE_SHARE_READ,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut info = MaybeUninit::uninit();
            // SAFETY: valid owned handle and output buffer.
            if unsafe { GetFileInformationByHandle(handle, info.as_mut_ptr()) } == 0 {
                let error = io::Error::last_os_error();
                // SAFETY: the failed information query did not close the handle.
                unsafe { CloseHandle(handle) };
                return Err(error);
            }
            // SAFETY: successful query initialized the entire structure.
            let pin = Self {
                handle,
                info: unsafe { info.assume_init() },
            };
            let attrs = pin.info.dwFileAttributes;
            if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
                || (attrs & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
                || (!directory && pin.info.nNumberOfLinks != 1)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "缓存含重解析点、硬链接或不匹配的文件类型",
                ));
            }
            Ok(pin)
        }

        fn delete(&self) -> Result<(), String> {
            // Delete by the already validated handle while its no-delete-sharing
            // lock remains held. Path-based remove_dir would conflict with our lock.
            let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: live handle with DELETE access, correctly sized input structure.
            if unsafe {
                SetFileInformationByHandle(
                    self.handle,
                    FileDispositionInfo,
                    (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            } == 0
            {
                return Err(io::Error::last_os_error().to_string());
            }
            Ok(())
        }
    }

    fn walk(
        path: &Path,
        pin: &Pin,
        clear: bool,
        budget: &mut usize,
        depth: usize,
    ) -> Result<u64, String> {
        if depth > MAX_DEPTH {
            return Err("缓存目录层级超出安全上限".into());
        }
        let mut total = 0u64;
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            consume(budget, depth)?;
            let entry = entry.map_err(|e| e.to_string())?;
            // The parent and all ancestors are pinned. This type hint is not a
            // security check: CreateFile's handle attributes are authoritative.
            let directory = entry.file_type().map_err(|e| e.to_string())?.is_dir();
            let child_path = entry.path();
            let child = Pin::open(&child_path, directory, clear).map_err(|e| e.to_string())?;
            if child.info.dwVolumeSerialNumber != pin.info.dwVolumeSerialNumber {
                return Err("拒绝跨卷遍历缓存".into());
            }
            let bytes = if directory {
                walk(&child_path, &child, clear, budget, depth + 1)?
            } else {
                (u64::from(child.info.nFileSizeHigh) << 32) | u64::from(child.info.nFileSizeLow)
            };
            if clear {
                child.delete()?;
            }
            total = total.checked_add(bytes).ok_or("缓存大小溢出")?;
            // Closing this handle completes deletion before parent removal.
        }
        Ok(total)
    }

    pub(super) fn run(path: &Path, clear: bool, remove_root: bool) -> Result<u64, String> {
        let mut components = path.components();
        let prefix = match components.next() {
            Some(Component::Prefix(p))
                if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) =>
            {
                p
            }
            _ => return Err("缓存路径必须为本地磁盘绝对路径".into()),
        };
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err("缓存路径必须为绝对路径".into());
        }
        let names: Vec<_> = components
            .map(|part| match part {
                Component::Normal(name) => {
                    // Prevent alternate streams, Win32 normalization and device names
                    // from making a checked component denote a different object.
                    let text = name.to_str().ok_or("缓存路径编码无效")?;
                    let stem = text.split('.').next().unwrap_or("").to_ascii_uppercase();
                    if text.contains(':')
                        || text.ends_with(['.', ' '])
                        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                        || (stem.len() == 4
                            && (stem.starts_with("COM") || stem.starts_with("LPT"))
                            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
                    {
                        return Err("缓存路径含不安全分量");
                    }
                    Ok(name)
                }
                _ => Err("缓存路径含不安全分量"),
            })
            .collect::<Result<_, _>>()?;
        if names.is_empty() {
            return Err("拒绝处理磁盘根目录".into());
        }
        let mut current = PathBuf::from(prefix.as_os_str());
        current.push("\\");
        let mut pins = vec![Pin::open(&current, true, false).map_err(|e| e.to_string())?];
        for (index, name) in names.iter().enumerate() {
            current.push(name);
            match Pin::open(
                &current,
                true,
                clear && remove_root && index + 1 == names.len(),
            ) {
                Ok(pin) => pins.push(pin),
                Err(e)
                    if matches!(e.raw_os_error(), Some(code)
                    if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PATH_NOT_FOUND as i32) =>
                {
                    return Ok(0);
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let root = pins.last().ok_or("缓存目录缺失")?;
        let mut budget = MAX_ENTRIES;
        let total = walk(&current, root, clear, &mut budget, 0)?;
        if clear && remove_root {
            root.delete()?;
        }
        Ok(total)
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub(super) fn run(_: &Path, _: bool, _: bool) -> Result<u64, String> {
        Err("此平台不支持安全缓存操作".into())
    }
}
