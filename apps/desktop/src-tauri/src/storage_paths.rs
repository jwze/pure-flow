// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Only fixed application-owned paths; frontend cannot supply deletion targets.
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::Manager;

#[derive(Serialize)]
pub struct StorageStatus {
    data_path: String,
    cache_path: String,
    database_bytes: u64,
    cache_bytes: u64,
    paused: bool,
    phase: String,
}
/// Windows: resolve once at startup. Never move/copy an existing roaming database.
#[cfg(windows)]
pub fn database_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let legacy = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("pureflow.sqlite3");
    if legacy.try_exists().map_err(|e| e.to_string())? {
        return Ok(legacy);
    }
    Ok(windows_root(app)?.join("pureflow.sqlite3"))
}

#[cfg(windows)]
fn windows_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let root = app.path().app_local_data_dir().map_err(|e| e.to_string())?;
    // On Windows both Tauri paths resolve to LocalAppData / identifier. Keep
    // WebView, new histories and the helper's protected runtime journal together.
    let cache = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    if root != cache || root.file_name().is_none_or(|v| v != "xyz.115613.pureflow") {
        return Err("Windows 应用目录解析不符合固定 LocalAppData 命名空间".into());
    }
    Ok(root)
}
pub fn webview_cache(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    #[cfg(windows)]
    return Ok(windows_root(app)?.join("webview"));
    #[cfg(not(windows))]
    Ok(app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("webview"))
}
pub fn status(app: &tauri::AppHandle) -> Result<StorageStatus, String> {
    #[cfg(not(windows))]
    let data = app.path().app_data_dir().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    let data = app
        .state::<crate::runtime::Monitor>()
        .database_path()
        .parent()
        .ok_or("数据库父目录缺失")?
        .to_path_buf();
    let cache = webview_cache(app)?;
    let tray = app.state::<crate::runtime::Monitor>().tray_status()?;
    #[cfg(not(windows))]
    let database_bytes = fs::metadata(data.join("pureflow.sqlite3"))
        .map_err(|e| e.to_string())?
        .len();
    #[cfg(windows)]
    let database_bytes = database_bytes(&data.join("pureflow.sqlite3"))?;
    Ok(StorageStatus {
        data_path: data.display().to_string(),
        cache_path: cache.display().to_string(),
        database_bytes,
        cache_bytes: crate::cache_fs::directory_bytes(&cache)?,
        paused: tray.paused,
        phase: tray.phase,
    })
}
#[cfg(windows)]
fn database_bytes(database: &Path) -> Result<u64, String> {
    let mut total = fs::metadata(database).map_err(|e| e.to_string())?.len();
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut path = database.as_os_str().to_os_string();
        path.push(suffix);
        match fs::symlink_metadata(PathBuf::from(path)) {
            Ok(metadata) => {
                use std::os::windows::fs::MetadataExt;
                if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
                    return Err("数据库事务文件含重解析点或类型异常，拒绝统计".into());
                }
                total = total.checked_add(metadata.len()).ok_or("数据库占用溢出")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(total)
}
pub fn clear_webview_cache(app: &tauri::AppHandle) -> Result<(), String> {
    #[cfg(not(windows))]
    let root = app.path().app_cache_dir().map_err(|e| e.to_string())?;
    // Fixed browser cache children only; pinned no-follow traversal cannot escape via replacement links.
    #[cfg(windows)]
    crate::cache_fs::clear_directory(&webview_cache(app)?, false)?;
    #[cfg(not(windows))]
    crate::cache_fs::clear_directory(&root.join("webview"), false)?;
    #[cfg(target_os = "macos")]
    crate::cache_fs::clear_directory(&root.join("WebKit"), true)?;
    Ok(())
}
pub fn ensure_compaction_space(database: &Path) -> Result<(), String> {
    let size = fs::metadata(database).map_err(|e| e.to_string())?.len();
    let required = size
        .checked_mul(2)
        .and_then(|v| v.checked_add(16 * 1024 * 1024))
        .ok_or("整理空间预算溢出")?;
    #[cfg(target_os = "macos")]
    let available = {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let path = CString::new(
            database
                .parent()
                .ok_or("数据库父目录缺失")?
                .as_os_str()
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: path is NUL terminated, statvfs writes the complete output on success.
        if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let info = unsafe { info.assume_init() };
        u64::from(info.f_bavail).saturating_mul(info.f_frsize)
    };
    #[cfg(windows)]
    let available = {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDiskFreeSpaceExW(
                path: *const u16,
                available: *mut u64,
                total: *mut u64,
                free: *mut u64,
            ) -> i32;
        }
        let path: Vec<u16> = database
            .parent()
            .ok_or("数据库父目录缺失")?
            .as_os_str()
            .encode_wide()
            .chain([0])
            .collect();
        let mut value = 0;
        // SAFETY: path is NUL terminated; valid u64 output, unused outputs are null.
        if unsafe {
            GetDiskFreeSpaceExW(
                path.as_ptr(),
                &mut value,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        value
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let available = 0;
    if available < required {
        return Err("磁盘可用空间不足，未整理历史文件".into());
    }
    Ok(())
}
