// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Explicit user action only: request the OS permission needed to read Wi-Fi SSIDs.
//! This module never starts location updates, reads coordinates, or reads an SSID.

#[cfg(target_os = "windows")]
pub fn request(app: &tauri::AppHandle) -> Result<String, String> {
    use tauri_plugin_opener::OpenerExt;

    // A fixed backend destination, not an arbitrary URL supplied through IPC.
    app.opener()
        .open_url("ms-settings:privacy-location", None::<&str>)
        .map_err(|error| format!("无法打开 Windows 位置隐私设置：{error}"))?;
    Ok("已打开位置隐私设置；请允许位置服务及桌面应用访问位置，然后重新采集。打开设置不代表已获授权。".into())
}

#[cfg(target_os = "macos")]
pub fn request(app: &tauri::AppHandle) -> Result<String, String> {
    use objc2::MainThreadMarker;
    use std::{sync::mpsc, time::Duration};

    // Tauri may invoke this synchronous API on either the main thread or an IPC
    // worker. Do not wait for a queued closure when already on the main thread.
    if let Some(main_thread) = MainThreadMarker::new() {
        return macos::request_on_main_thread(main_thread);
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let result = MainThreadMarker::new()
            .ok_or_else(|| "位置授权请求未在 macOS 主线程执行".to_string())
            .and_then(macos::request_on_main_thread);
        // The caller can time out; the retained manager still stays on the main
        // thread and a later IPC call can query the actual authorization status.
        let _ = sender.send(result);
    })
    .map_err(|error| format!("无法调度位置授权请求：{error}"))?;
    receiver
        .recv_timeout(Duration::from_secs(5))
        .map_err(|error| format!("位置授权请求状态尚未取得，请稍后重试：{error}"))?
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn request(_app: &tauri::AppHandle) -> Result<String, String> {
    Err("当前平台不支持此 Wi-Fi 名称隐私授权操作".into())
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::{rc::Retained, MainThreadMarker};
    use objc2_core_location::{CLAuthorizationStatus, CLLocationManager};
    use std::cell::RefCell;

    struct PermissionRequest {
        manager: Retained<CLLocationManager>,
        requested: bool,
    }

    thread_local! {
        // Accessed only after obtaining MainThreadMarker. The manager survives
        // the IPC that triggers the asynchronous system prompt. No ObjC object
        // is moved into a Send closure or stored in Tauri's Send + Sync state.
        static PERMISSION: RefCell<Option<PermissionRequest>> = const { RefCell::new(None) };
    }

    pub(super) fn request_on_main_thread(_main_thread: MainThreadMarker) -> Result<String, String> {
        PERMISSION.with(|slot| {
            let mut slot = slot.borrow_mut();
            let permission = slot.get_or_insert_with(|| PermissionRequest {
                // SAFETY: creation and all subsequent use occur on the main
                // thread, which has Tauri's running application event loop.
                manager: unsafe { CLLocationManager::new() },
                requested: false,
            });
            // SAFETY: CoreLocation queries run on the same main thread as the
            // persistent manager. Neither query requests any location data.
            if !unsafe { CLLocationManager::locationServicesEnabled_class() } {
                return Ok("disabled：系统位置服务已关闭；请在系统设置中启用位置服务后重试。".into());
            }
            let status = unsafe { permission.manager.authorizationStatus() };
            if status == CLAuthorizationStatus::NotDetermined {
                if !permission.requested {
                    // Set this before requesting; repeated IPCs while the
                    // system sheet is open must not issue another request.
                    permission.requested = true;
                    // SAFETY: user explicitly invoked this API; Info.plist
                    // contains the location usage descriptions. This requests
                    // only when-in-use permission, never location updates.
                    unsafe { permission.manager.requestWhenInUseAuthorization() };
                }
                return Ok("pending：位置授权异步等待系统处理；请完成系统提示后再次检查授权并重新采集，当前尚未确认授权。".into());
            }
            Ok(status_message(status).into())
        })
    }

    fn status_message(status: CLAuthorizationStatus) -> &'static str {
        match status {
            CLAuthorizationStatus::AuthorizedAlways | CLAuthorizationStatus::AuthorizedWhenInUse =>
                "authorized：系统位置权限已允许；请重新采集 Wi-Fi 名称。权限不保证 SSID 可读，实际结果以采集器诊断为准。",
            CLAuthorizationStatus::Denied =>
                "denied：位置权限已被拒绝；请在系统设置 → 隐私与安全性 → 定位服务中允许 PureFlow，然后再次检查授权并重新采集。",
            CLAuthorizationStatus::Restricted =>
                "restricted：位置权限受到系统策略限制；本应用不会提升权限或绕过限制，请检查系统策略后重试。",
            _ => "unknown：系统位置权限状态无法确认；请检查系统设置后重试，尚未确认授权。",
        }
    }
}
