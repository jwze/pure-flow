// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Crash recovery journal. Never enumerate/stop sessions by prefix: only a
//! protected exact owner record, dead process identity and queried GUID permit
//! stopping the exact queried handle. All failures are fail-closed.
use super::{unavailable, AlignedBuffer, CollectorError};
use std::{
    ffi::c_void,
    fs::File,
    io::{Read, Write},
    mem::size_of,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
};
use windows::{
    core::{GUID, PCWSTR},
    Win32::{Foundation::ERROR_SUCCESS, System::Diagnostics::Etw::*},
};

// Narrow declarations avoid expanding the collector's Windows feature surface.
// These APIs/structures are stable Win32 ABI, called only on Windows.
type Handle = *mut c_void;
#[repr(C)]
struct SecurityAttributes {
    size: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}
impl FileTime {
    fn value(&self) -> u64 {
        ((self.high as u64) << 32) | self.low as u64
    }
}
#[repr(C)]
#[derive(Default)]
struct FileInfo {
    attributes: u32,
    creation: FileTime,
    access: FileTime,
    write: FileTime,
    volume: u32,
    size_high: u32,
    size_low: u32,
    links: u32,
    index_high: u32,
    index_low: u32,
}
#[repr(C)]
struct Acl {
    revision: u8,
    reserved: u8,
    size: u16,
    count: u16,
    reserved2: u16,
}
#[repr(C)]
struct Ace {
    kind: u8,
    flags: u8,
    size: u16,
    mask: u32,
    sid: u32,
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        path: *const u16,
        access: u32,
        share: u32,
        security: *const SecurityAttributes,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn CreateDirectoryW(path: *const u16, security: *const SecurityAttributes) -> i32;
    fn GetFileInformationByHandle(file: Handle, info: *mut FileInfo) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    fn GetLastError() -> u32;
    fn LocalFree(memory: Handle) -> Handle;
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentProcessId() -> u32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn GetProcessTimes(
        process: Handle,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn WaitForSingleObject(handle: Handle, timeout: u32) -> u32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        info: *mut c_void,
        len: u32,
        needed: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: Handle, text: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut Handle,
        size: *mut u32,
    ) -> i32;
    fn GetSecurityInfo(
        object: Handle,
        kind: u32,
        flags: u32,
        owner: *mut Handle,
        group: *mut Handle,
        dacl: *mut *mut Acl,
        sacl: *mut Handle,
        descriptor: *mut Handle,
    ) -> u32;
    fn IsWellKnownSid(sid: Handle, kind: u32) -> i32;
    fn GetAce(acl: *const Acl, index: u32, ace: *mut Handle) -> i32;
    fn GetSecurityDescriptorControl(
        descriptor: Handle,
        control: *mut u16,
        revision: *mut u32,
    ) -> i32;
}
#[link(name = "shell32")]
unsafe extern "system" {
    fn SHGetKnownFolderPath(id: *const GUID, flags: u32, token: Handle, path: *mut *mut u16)
        -> i32;
}
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemFree(memory: Handle);
}

struct OwnedHandle(Handle);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
// Owned directory locks move with the collector and are not used concurrently.
unsafe impl Send for OwnedHandle {}
struct Descriptor(Handle);
// LocalAlloc-owned immutable descriptor, transferred with its owning session.
unsafe impl Send for Descriptor {}
impl Drop for Descriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}
fn error(label: &str) -> CollectorError {
    unavailable(format!("{label}: Windows error {}", unsafe {
        GetLastError()
    }))
}
fn creation(process: Handle) -> Result<u64, CollectorError> {
    let (mut birth, mut exit, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    if unsafe { GetProcessTimes(process, &mut birth, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(error("Owner process creation time"));
    }
    Ok(birth.value())
}
fn security() -> Result<Descriptor, CollectorError> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } == 0 {
        return Err(error("Owner token"));
    }
    let token = OwnedHandle(token);
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token.0, 1, null_mut(), 0, &mut needed);
    }
    if needed == 0 || needed > 65536 {
        return Err(unavailable("Invalid owner SID length"));
    }
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    if unsafe { GetTokenInformation(token.0, 1, storage.as_mut_ptr().cast(), needed, &mut needed) }
        == 0
    {
        return Err(error("Owner SID"));
    }
    // TOKEN_USER begins with SID_AND_ATTRIBUTES, whose first member is PSID.
    let sid = unsafe { *storage.as_ptr().cast::<Handle>() };
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(error("Owner SID string"));
    }
    let memory = Descriptor(text.cast());
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(text, len)) };
    drop(memory);
    // An unelevated same-user process must NOT be able to forge a privileged
    // cleanup instruction. Owner is Administrators, user gets read/traverse only.
    let sddl: Vec<u16> = format!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;{sid})")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor = null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(error("Owner journal ACL"));
    }
    Ok(Descriptor(descriptor))
}
fn validate_acl(handle: Handle) -> Result<(), CollectorError> {
    let (mut owner, mut descriptor, mut acl) = (null_mut(), null_mut(), null_mut());
    let status = unsafe {
        GetSecurityInfo(
            handle,
            1,
            1 | 4,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(unavailable(format!("Read owner journal ACL: {status}")));
    }
    let _descriptor = Descriptor(descriptor);
    // Builtin Administrators or LocalSystem, never the unelevated user owner.
    let trusted = |sid| unsafe { IsWellKnownSid(sid, 26) != 0 || IsWellKnownSid(sid, 22) != 0 };
    let mut control = 0;
    let mut revision = 0;
    if !trusted(owner)
        || acl.is_null()
        || unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
        || control & 0x1000 == 0
    {
        return Err(unavailable(
            "Untrusted/unprotected ETW owner journal; refusing recovery",
        ));
    }
    // Permit only simple allow ACEs; untrusted principals may read/traverse but
    // cannot write, append, delete children, delete, change DACL or take ownership.
    let writable = 0x1000_0000 | 0x4000_0000 | 0x000d_0156;
    let count = unsafe { (*acl).count };
    for index in 0..count as u32 {
        let mut pointer = null_mut();
        if unsafe { GetAce(acl, index, &mut pointer) } == 0 {
            return Err(error("Owner journal ACE"));
        }
        let ace = unsafe { &*pointer.cast::<Ace>() };
        if ace.kind != 0
            || ace.size < size_of::<Ace>() as u16
            || (!trusted(std::ptr::addr_of!(ace.sid).cast_mut().cast()) && ace.mask & writable != 0)
        {
            return Err(unavailable(
                "Writable/unsupported ETW owner journal ACL; refusing recovery",
            ));
        }
    }
    Ok(())
}
fn open(
    path: &Path,
    directory: bool,
    create: Option<&SecurityAttributes>,
) -> Result<OwnedHandle, CollectorError> {
    let handle = unsafe {
        CreateFileW(
            wide(path).as_ptr(),
            if directory {
                0x0002_0080 // READ_CONTROL | FILE_READ_ATTRIBUTES; no directory data access.
            } else if create.is_some() {
                0xc002_0000
            } else {
                0x8002_0000
            },
            // Directory locks deny rename/delete, not normal file creation or
            // unrelated user writes in profile ancestors. The journal DACL
            // separately prevents unprivileged changes to its contents.
            if create.is_some() { 1 } else { 3 },
            create.map_or(null(), |v| v),
            if create.is_some() { 1 } else { 3 },
            0x0020_0000 | if directory { 0x0200_0000 } else { 0 },
            null_mut(),
        )
    };
    if handle as isize == -1 {
        return Err(error("Open ETW owner journal (no reparse/share-delete)"));
    }
    let handle = OwnedHandle(handle);
    let mut info = FileInfo::default();
    if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
        return Err(error("ETW owner journal attributes"));
    }
    if info.attributes & 0x400 != 0
        || (info.attributes & 0x10 != 0) != directory
        || (!directory && info.links != 1)
    {
        return Err(unavailable(
            "ETW owner journal reparse point/hard link/type rejected",
        ));
    }
    Ok(handle)
}
struct Store {
    dir: PathBuf,
    _locks: Vec<OwnedHandle>,
    descriptor: Descriptor,
}
impl Store {
    fn open() -> Result<Self, CollectorError> {
        let mut path = null_mut();
        let id = GUID::from_u128(0xf1b32785_6fba_4fcf_9d55_7b8e7f157091); // LocalAppData, not environment input.
        let status = unsafe { SHGetKnownFolderPath(&id, 0, null_mut(), &mut path) };
        if status < 0 {
            return Err(unavailable(format!("LocalAppData lookup: {status}")));
        }
        let mut len = 0;
        unsafe {
            while *path.add(len) != 0 {
                len += 1;
            }
        }
        let root = PathBuf::from(unsafe {
            String::from_utf16_lossy(std::slice::from_raw_parts(path, len))
        });
        unsafe {
            CoTaskMemFree(path.cast());
        }
        if !root.is_absolute()
            || !matches!(root.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        {
            return Err(unavailable(
                "Owner journal requires local absolute LocalAppData (no UNC/device paths)",
            ));
        }
        let descriptor = security()?;
        let attributes = SecurityAttributes {
            size: size_of::<SecurityAttributes>() as u32,
            descriptor: descriptor.0,
            inherit: 0,
        };
        let mut locks = Vec::new();
        let mut current = PathBuf::new();
        for component in root.components() {
            current.push(component);
            if current.has_root() {
                locks.push(open(&current, true, None)?);
            }
        }
        for name in ["xyz.115613.pureflow", "runtime", "etw-owners"] {
            current.push(name);
            // Do not make the shared application/cache namespace read-only to
            // the GUI. Only the dedicated ownership directory is privileged.
            let security: *const SecurityAttributes = if name == "etw-owners" {
                &attributes
            } else {
                null()
            };
            if unsafe { CreateDirectoryW(wide(&current).as_ptr(), security) } == 0
                && unsafe { GetLastError() } != 183
            {
                return Err(error("Create ETW owner journal directory"));
            }
            let lock = open(&current, true, None)?;
            if name == "etw-owners" {
                validate_acl(lock.0)?;
            }
            locks.push(lock);
        }
        Ok(Self {
            dir: current,
            _locks: locks,
            descriptor,
        })
    }
    fn recover(&self) -> Result<(), CollectorError> {
        // Bound startup work and reject unbounded journals; never scan all ETW sessions.
        let entries = std::fs::read_dir(&self.dir).map_err(unavailable)?;
        for (index, entry) in entries.enumerate() {
            if index >= 128 {
                return Err(unavailable("ETW owner journal limit exceeded"));
            }
            let path = entry.map_err(unavailable)?.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let extension = path.extension().and_then(|s| s.to_str());
            if !matches!(extension, Some("owner" | "pending"))
                || stem.len() != 32
                || !stem.bytes().all(|b| b.is_ascii_hexdigit())
            {
                continue;
            }
            let handle = open(&path, false, None)?;
            validate_acl(handle.0)?;
            if extension == Some("pending") {
                // ETW can only start after the durable record is published as
                // .owner. A .pending file is an interrupted pre-start write.
                drop(handle);
                std::fs::remove_file(&path).map_err(unavailable)?;
                continue;
            }
            let file = unsafe { File::from_raw_handle(handle.0) };
            std::mem::forget(handle);
            let mut text = String::new();
            (&file)
                .take(1025)
                .read_to_string(&mut text)
                .map_err(unavailable)?;
            if text.len() > 1024 {
                return Err(unavailable("ETW owner record too large"));
            }
            let record = Record::parse(&text)?;
            if format!("{:032x}", record.guid.to_u128()) != stem {
                return Err(unavailable("ETW owner filename/GUID mismatch"));
            }
            if !record.dead()? {
                // A retained record from a live helper means its cleanup has
                // not been proven, even if its IPC/lease already disappeared.
                // Never overlap an active or failed-cleanup privileged session.
                return Err(unavailable(
                    "Recorded ETW owner is still alive; refusing collector replacement",
                ));
            }
            if record.stop()? {
                drop(file);
                std::fs::remove_file(&path).map_err(unavailable)?;
            }
        }
        Ok(())
    }
}
struct Record {
    name: String,
    guid: GUID,
    pid: u32,
    birth: u64,
}
impl Record {
    fn parse(text: &str) -> Result<Self, CollectorError> {
        let lines: Vec<_> = text.lines().collect();
        if lines.len() != 4 {
            return Err(unavailable("Invalid ETW owner record"));
        }
        let guid = u128::from_str_radix(lines[1], 16).map_err(unavailable)?;
        let pid = lines[2].parse::<u32>().map_err(unavailable)?;
        let birth = lines[3].parse::<u64>().map_err(unavailable)?;
        if pid == 0 || birth == 0 || lines[0] != format!("PureFlowCollector-{pid}-{guid:032x}") {
            return Err(unavailable("Invalid exact ETW owner identity"));
        }
        Ok(Self {
            name: lines[0].into(),
            guid: GUID::from_u128(guid),
            pid,
            birth,
        })
    }
    fn dead(&self) -> Result<bool, CollectorError> {
        let process = unsafe { OpenProcess(0x0010_1000, 0, self.pid) };
        if process.is_null() {
            // INVALID_PARAMETER means no such PID; ACCESS_DENIED is NOT death.
            return if unsafe { GetLastError() } == 87 {
                Ok(true)
            } else {
                Err(error("Validate ETW owner process"))
            };
        }
        let process = OwnedHandle(process);
        let birth = creation(process.0)?;
        Ok(birth != self.birth || unsafe { WaitForSingleObject(process.0, 0) } == 0)
    }
    fn stop(&self) -> Result<bool, CollectorError> {
        let name: Vec<_> = self.name.encode_utf16().chain([0]).collect();
        let mut buffer = AlignedBuffer::new(size_of::<EVENT_TRACE_PROPERTIES>() + 2048);
        let properties = buffer.ptr::<EVENT_TRACE_PROPERTIES>();
        unsafe {
            (*properties).Wnode.BufferSize = buffer.len as u32;
            (*properties).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            (*properties).LogFileNameOffset = (size_of::<EVENT_TRACE_PROPERTIES>() + 1024) as u32;
        }
        let status = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PCWSTR(name.as_ptr()),
                properties,
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        if status.0 == 4201 {
            return Ok(true);
        } // ERROR_WMI_INSTANCE_NOT_FOUND
        if status != ERROR_SUCCESS {
            return Err(unavailable(format!(
                "Query recorded ETW session: {}",
                status.0
            )));
        }
        let offset = unsafe { (*properties).LoggerNameOffset } as usize;
        if offset < size_of::<EVENT_TRACE_PROPERTIES>()
            || !offset.is_multiple_of(2)
            || offset >= buffer.len
        {
            return Err(unavailable(
                "Invalid queried ETW name; session left untouched",
            ));
        }
        let bytes = &buffer.bytes()[offset..];
        let queried: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_le_bytes(*v))
            .take_while(|v| *v != 0)
            .collect();
        if queried.len() * 2 >= bytes.len()
            || String::from_utf16(&queried).ok().as_deref() != Some(self.name.as_str())
            || unsafe { (*properties).Wnode.Guid } != self.guid
        {
            return Err(unavailable(
                "Recorded ETW name/GUID mismatch; session left untouched",
            ));
        }
        // Use the queried session handle, not the name, to avoid a replacement
        // session between query and stop. Never target a foreign/system session.
        let control = CONTROLTRACE_HANDLE {
            Value: unsafe { (*properties).Wnode.Anonymous1.HistoricalContext },
        };
        let status = unsafe {
            ControlTraceW(
                control,
                PCWSTR::null(),
                properties,
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        if status != ERROR_SUCCESS && status.0 != 4201 {
            return Err(unavailable(format!(
                "Stop exact recorded ETW session: {}",
                status.0
            )));
        }
        Ok(true)
    }
}

pub(super) struct Owner {
    path: PathBuf,
    file: Option<File>,
    _store: Store,
}
impl Owner {
    pub fn prepare(guid: GUID) -> Result<(Self, String), CollectorError> {
        let store = Store::open()?;
        store.recover()?;
        let pid = unsafe { GetCurrentProcessId() };
        let birth = creation(unsafe { GetCurrentProcess() })?;
        let name = format!("PureFlowCollector-{pid}-{:032x}", guid.to_u128());
        let path = store.dir.join(format!("{:032x}.owner", guid.to_u128()));
        let attributes = SecurityAttributes {
            size: size_of::<SecurityAttributes>() as u32,
            descriptor: store.descriptor.0,
            inherit: 0,
        };
        let pending = path.with_extension("pending");
        let handle = open(&pending, false, Some(&attributes))?;
        validate_acl(handle.0)?;
        let mut file = unsafe { File::from_raw_handle(handle.0) };
        std::mem::forget(handle);
        write!(file, "{name}\n{:032x}\n{pid}\n{birth}\n", guid.to_u128()).map_err(unavailable)?;
        file.sync_all().map_err(unavailable)?;
        drop(file);
        // MOVEFILE_WRITE_THROUGH: publish only a complete, durable record.
        // Do not replace an existing owner file even on a GUID collision.
        if unsafe { MoveFileExW(wide(&pending).as_ptr(), wide(&path).as_ptr(), 8) } == 0 {
            return Err(error("Publish ETW owner record"));
        }
        let handle = open(&path, false, None)?;
        validate_acl(handle.0)?;
        let file = unsafe { File::from_raw_handle(handle.0) };
        std::mem::forget(handle);
        // Durable ownership exists BEFORE StartTrace; a crash at any subsequent
        // point can be recovered. A crash here merely leaves a harmless record.
        Ok((
            Self {
                path,
                file: Some(file),
                _store: store,
            },
            name,
        ))
    }
    pub fn remove(&mut self) -> Result<(), CollectorError> {
        self.file.take();
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(unavailable(format!(
                "ETW owner journal cleanup failed (record retained): {error}"
            ))),
        }
    }
}
