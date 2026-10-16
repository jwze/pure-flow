// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Bounded macOS native reads, using libc's SDK layouts rather than text tools.

use crate::CollectorError;
use pureflow_core::{Bytes, InterfaceCounter};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    mem::{offset_of, size_of, MaybeUninit},
    ptr,
};

const MAX_IFLIST: usize = 4 * 1024 * 1024;
const MAX_NAMES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Birth {
    pub sec: u64,
    pub usec: u64,
}
#[derive(Debug)]
pub(super) struct ProcessName {
    pub name: String,
    pub birth: Birth,
}
#[derive(Clone, Copy, Eq, PartialEq)]
struct ProcessInfo {
    birth: Birth,
    comm: [libc::c_char; libc::MAXCOMLEN],
    name: [libc::c_char; 32],
}
struct CachedName {
    name: String,
    info: ProcessInfo,
}
#[derive(Default)]
pub(super) struct ProcessResolver {
    cache: HashMap<(u32, Birth), CachedName>,
    retained_bytes: usize,
}
pub(super) struct ResolvedBatch {
    pub names: HashMap<u32, ProcessName>,
    pub races: u64,
}
impl ProcessResolver {
    pub(super) fn resolve_batch(&mut self, pids: impl Iterator<Item = u32>) -> ResolvedBatch {
        let mut names = HashMap::new();
        let mut races = 0;
        for pid in pids {
            let Some(before) = process_info(pid) else {
                continue;
            };
            let key = (pid, before.birth);
            // exec preserves birth. The already-read BSD comm/name signature
            // invalidates cached paths when that process changes its image.
            if self
                .cache
                .get(&key)
                .is_some_and(|cached| cached.info != before)
            {
                if let Some(old) = self.cache.remove(&key) {
                    self.retained_bytes -= old.name.len() + size_of::<((u32, Birth), CachedName)>();
                }
            }
            let name = if let Some(cached) = self.cache.get(&key) {
                cached.name.clone()
            } else {
                let Some(path) = process_path(pid) else {
                    continue;
                };
                let Some(name) = display_name(&path) else {
                    continue;
                };
                name
            };
            // Revalidate even cached entries: lookup/path lookup is a separate
            // snapshot. Exits/reuse/exec during this window are omissions, not
            // names borrowed from another process/image with the same PID.
            if process_info(pid) != Some(before) {
                races += 1;
                continue;
            }
            if !self.cache.contains_key(&key) && self.cache.len() < MAX_NAMES {
                let size = name.len() + size_of::<((u32, Birth), CachedName)>();
                if self.retained_bytes + size <= 8 * 1024 * 1024 {
                    self.cache.insert(
                        key,
                        CachedName {
                            name: name.clone(),
                            info: before,
                        },
                    );
                    self.retained_bytes += size;
                }
            }
            names.insert(
                pid,
                ProcessName {
                    name,
                    birth: before.birth,
                },
            );
        }
        // Retain only this batch's validated identities. This bounds churn and
        // promptly drops stale PID/birth names without refreshing stable paths.
        self.cache
            .retain(|(pid, birth), _| names.get(pid).is_some_and(|name| name.birth == *birth));
        self.retained_bytes = self
            .cache
            .values()
            .map(|cached| cached.name.len() + size_of::<((u32, Birth), CachedName)>())
            .sum();
        ResolvedBatch { names, races }
    }
}
fn unavailable(message: impl ToString) -> CollectorError {
    CollectorError::Unavailable(message.to_string())
}
fn process_info(pid: u32) -> Option<ProcessInfo> {
    if pid == 0 {
        return None;
    }
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let expected = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: libproc receives a writable buffer of exactly its SDK structure
    // size. Assume initialization only if it returns that exact size.
    let count = unsafe {
        libc::proc_pidinfo(
            libc::pid_t::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            expected,
        )
    };
    if count != expected {
        return None;
    }
    let info = unsafe { info.assume_init() };
    if info.pbi_pid != pid || info.pbi_start_tvsec == 0 || info.pbi_start_tvusec >= 1_000_000 {
        return None;
    }
    Some(ProcessInfo {
        birth: Birth {
            sec: info.pbi_start_tvsec,
            usec: info.pbi_start_tvusec,
        },
        comm: info.pbi_comm,
        name: info.pbi_name,
    })
}
fn process_path(pid: u32) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the bounded allocation is writable for the supplied size.
    let count = unsafe {
        libc::proc_pidpath(
            libc::pid_t::try_from(pid).ok()?,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).ok()?,
        )
    };
    let count = usize::try_from(count).ok()?;
    if count == 0 || count >= buffer.len() {
        return None;
    }
    buffer.truncate(count);
    Some(buffer)
}
fn display_name(path: &[u8]) -> Option<String> {
    let path = std::str::from_utf8(path).ok()?.trim_end_matches('\0');
    let path = path.split_once(".app/").map_or(path, |(prefix, _)| prefix);
    let name = path.rsplit('/').next().unwrap_or(path);
    (!name.is_empty()).then(|| name.to_owned())
}

pub(super) fn interfaces() -> Result<Vec<InterfaceCounter>, CollectorError> {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    // Interface changes can enlarge the buffer between sysctl's two snapshots.
    // Retry that bounded race, never allocate the kernel-reported size blindly.
    for _ in 0..3 {
        let mut length = 0usize;
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                ptr::null_mut(),
                &mut length,
                ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(unavailable(io::Error::last_os_error()));
        }
        if length == 0 || length > MAX_IFLIST {
            return Err(unavailable(
                "NET_RT_IFLIST2 returned an invalid bounded size",
            ));
        }
        let mut bytes = vec![0u8; length];
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                bytes.as_mut_ptr().cast(),
                &mut length,
                ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOMEM) {
                continue;
            }
            return Err(unavailable(error));
        }
        if length > bytes.len() {
            return Err(unavailable("NET_RT_IFLIST2 exceeded its buffer"));
        }
        bytes.truncate(length);
        return parse_interfaces(&bytes);
    }
    Err(unavailable("NET_RT_IFLIST2 kept changing size"))
}
fn parse_interfaces(bytes: &[u8]) -> Result<Vec<InterfaceCounter>, CollectorError> {
    let mut cursor = 0usize;
    let mut interfaces = BTreeMap::new();
    while cursor < bytes.len() {
        if bytes.len() - cursor < 4 {
            return Err(unavailable("NET_RT_IFLIST2 has a truncated message header"));
        }
        let msglen = usize::from(u16::from_ne_bytes(
            bytes[cursor..cursor + 2].try_into().unwrap(),
        ));
        if msglen < 4 || msglen > bytes.len() - cursor {
            return Err(unavailable("NET_RT_IFLIST2 has an invalid message length"));
        }
        let message = &bytes[cursor..cursor + msglen];
        if message[2] != libc::RTM_VERSION as u8 {
            return Err(unavailable(
                "NET_RT_IFLIST2 has an unsupported route version",
            ));
        }
        if message[3] == libc::RTM_IFINFO2 as u8 {
            if message.len() < size_of::<libc::if_msghdr2>() {
                return Err(unavailable(
                    "NET_RT_IFLIST2 has a truncated interface header",
                ));
            }
            // SAFETY: length is checked above; routing messages are packed and
            // can start unaligned. SDK's packed(4) if_data64 fields are copied
            // unaligned as well, never referenced through an aligned borrow.
            let header =
                unsafe { ptr::read_unaligned(message.as_ptr().cast::<libc::if_msghdr2>()) };
            let received = unsafe { ptr::addr_of!(header.ifm_data.ifi_ibytes).read_unaligned() };
            let sent = unsafe { ptr::addr_of!(header.ifm_data.ifi_obytes).read_unaligned() };
            let index = header.ifm_index;
            if let Some(name) = interface_name(message, header.ifm_addrs as u32)? {
                if name.starts_with("en")
                    && interfaces
                        .insert(
                            name.clone(),
                            InterfaceCounter {
                                id: format!("{name}:{index}"),
                                name,
                                bytes: Bytes { received, sent },
                            },
                        )
                        .is_some()
                {
                    return Err(unavailable("NET_RT_IFLIST2 has duplicate interface names"));
                }
            }
        }
        cursor += msglen;
    }
    Ok(interfaces.into_values().collect())
}
fn interface_name(message: &[u8], addrs: u32) -> Result<Option<String>, CollectorError> {
    let mut offset = size_of::<libc::if_msghdr2>();
    for bit in 0..libc::RTAX_MAX {
        if addrs & (1 << bit) == 0 {
            continue;
        }
        let Some(&len) = message.get(offset) else {
            return Err(unavailable("NET_RT_IFLIST2 has a truncated address"));
        };
        let len = usize::from(len);
        if len < 2 || len > message.len() - offset {
            return Err(unavailable("NET_RT_IFLIST2 has an invalid address length"));
        }
        if bit == libc::RTAX_IFP {
            let start = offset + offset_of!(libc::sockaddr_dl, sdl_data);
            if start > offset + len || message[offset + 1] != libc::AF_LINK as u8 {
                return Err(unavailable("NET_RT_IFLIST2 has an invalid link address"));
            }
            let name_len = usize::from(message[offset + offset_of!(libc::sockaddr_dl, sdl_nlen)]);
            let end = start + name_len;
            if end > offset + len {
                return Err(unavailable("NET_RT_IFLIST2 has a truncated interface name"));
            }
            let name = std::str::from_utf8(&message[start..end])
                .map_err(|_| unavailable("NET_RT_IFLIST2 interface name is not UTF-8"))?;
            return Ok((!name.is_empty()).then(|| name.to_owned()));
        }
        // Darwin route sockaddrs are rounded to 32-bit boundaries, not the
        // native pointer width; long-alignment would skip names on arm64.
        offset += (len + 3) & !3;
    }
    Ok(None)
}
