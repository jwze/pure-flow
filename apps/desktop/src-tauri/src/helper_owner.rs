// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! A kernel-object lifetime lease, not a thread-owned mutex. The helper retains
//! it while paused and until explicit collector cleanup has completed. Names
//! come from the authenticated GUI token, never command-line identity claims.
use super::{os_error, token, wide, Handle, LocalMemory};
use std::{ffi::c_void, mem::size_of, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::SYNCHRONIZE,
    System::Threading::*,
};

struct Identity {
    sid: String,
    session: u32,
    logon: LUID,
}
fn information<T: Default>(token: &Handle, class: TOKEN_INFORMATION_CLASS) -> Result<T, String> {
    let mut value = T::default();
    let mut needed = 0;
    if unsafe {
        GetTokenInformation(
            token.0,
            class,
            (&mut value as *mut T).cast::<c_void>(),
            size_of::<T>() as u32,
            &mut needed,
        )
    } == 0
        || needed as usize != size_of::<T>()
    {
        return Err(os_error("Collector ownership token information"));
    }
    Ok(value)
}
fn identity(token: &Handle) -> Result<Identity, String> {
    let mut needed = 0;
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed) };
    if needed == 0 || needed > 65536 {
        return Err("Invalid collector ownership SID size".into());
    }
    let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(os_error("Collector ownership SID"));
    }
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return Err(os_error("Collector ownership SID string"));
    }
    let _memory = LocalMemory(text.cast());
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(text, len)) };
    let statistics: TOKEN_STATISTICS = information(token, TokenStatistics)?;
    Ok(Identity {
        sid,
        session: information(token, TokenSessionId)?,
        logon: statistics.AuthenticationId,
    })
}
impl Identity {
    fn name(&self) -> Vec<u16> {
        wide(&format!(
            "Local\\PureFlow-Collector-{}-{}-{:08x}{:08x}",
            self.sid, self.session, self.logon.HighPart as u32, self.logon.LowPart
        ))
    }
}
const ACTIVE: &str =
    "当前用户登录会话已有采集器（可能仍在暂停或清理）；已拒绝启动第二个采集器。请等待其退出后点击重新采集";

/// Advisory preflight avoids another UAC prompt while a previous elevated helper
/// is still alive. CreateMutexEx in the helper is the authoritative atomic check.
pub(super) fn preflight() -> Result<(), String> {
    let identity = identity(&token()?)?;
    let handle = unsafe { OpenMutexW(SYNCHRONIZE, 0, identity.name().as_ptr()) };
    if !handle.is_null() {
        drop(Handle(handle));
        return Err(ACTIVE.into());
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_FILE_NOT_FOUND {
        return Err(format!(
            "Collector ownership preflight failed (refusing UAC): {error}"
        ));
    }
    Ok(())
}

pub(super) struct Lease {
    _handle: Handle,
}
impl Lease {
    pub fn acquire(parent: &Handle) -> Result<Self, String> {
        let mut parent_token = null_mut();
        if unsafe { OpenProcessToken(parent.0, TOKEN_QUERY, &mut parent_token) } == 0 {
            return Err(os_error("GUI ownership token"));
        }
        let parent_identity = identity(&Handle(parent_token))?;
        let helper_identity = identity(&token()?)?;
        if parent_identity.sid != helper_identity.sid
            || parent_identity.session != helper_identity.session
        {
            return Err("Collector requires same-user, same-session UAC elevation".into());
        }
        // UAC linked tokens can have different AuthenticationIds. Both GUI and
        // helper key the lease with the ordinary GUI's verified logon identity.
        // The unelevated user can detect existence but cannot modify this lease.
        let sddl = wide(&format!(
            "O:BAG:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00100000;;;{})",
            parent_identity.sid
        ));
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
            return Err(os_error("Collector ownership DACL"));
        }
        let _memory = LocalMemory(descriptor);
        let security = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // No mutex ownership: Windows releases this lease only when its last
        // handle closes, including helper crash. No cross-thread ReleaseMutex.
        let handle =
            unsafe { CreateMutexExW(&security, parent_identity.name().as_ptr(), 0, SYNCHRONIZE) };
        let error = unsafe { GetLastError() };
        if handle.is_null() {
            return Err(format!("Collector ownership acquisition failed: {error}"));
        }
        let handle = Handle(handle);
        if error == ERROR_ALREADY_EXISTS {
            return Err(ACTIVE.into());
        }
        Ok(Self { _handle: handle })
    }
}
