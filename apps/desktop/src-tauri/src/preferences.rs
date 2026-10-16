// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! User-controlled login startup; no default enable and no separate preferences file.
//!
//! Initialize tauri-plugin-autostart v2 with `MacosLauncher::LaunchAgent` and
//! **empty args**. Login launches the ordinary GUI entry point, which must start
//! the collector helper immediately and request Windows UAC normally. Do not
//! add silent/no-UAC flags, scheduled tasks, or elevate the WebView process.

use serde::Serialize;
use tauri_plugin_autostart::ManagerExt;

#[derive(Debug, Serialize)]
pub struct Preferences {
    pub autostart: bool,
    #[cfg(windows)]
    pub updates_via_releases_page: bool,
}

/// Read the OS registration, without enabling startup as a side effect.
pub fn get(app: &tauri::AppHandle) -> Result<Preferences, String> {
    app.autolaunch()
        .is_enabled()
        .map(|autostart| Preferences {
            autostart,
            #[cfg(windows)]
            updates_via_releases_page: true,
        })
        .map_err(|error| format!("无法读取登录启动设置：{error}"))
}

/// Call only after the user explicitly chooses the desired setting.
/// Return the actual registration state, not an optimistic local preference.
pub fn set(app: &tauri::AppHandle, enabled: bool) -> Result<Preferences, String> {
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable()
    } else {
        launcher.disable()
    }
    .map_err(|error| format!("无法修改登录启动设置：{error}"))?;
    get(app)
}
