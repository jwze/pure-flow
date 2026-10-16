// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Windows has no update HTTP client: only an explicit, fixed release-page action.
use serde::Serialize;
use tauri_plugin_opener::OpenerExt;

pub const RELEASES_PAGE: &str = "https://115613.xyz/gitea/jvz/pure-flow/releases";

#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub available: bool,
    pub url: Option<String>,
    pub status: String,
}

// Preserve the existing IPC shape without making a network request or claiming
// that an unqueried version is current. The Windows UI opens the page directly.
pub fn check(current_version: &str) -> Result<UpdateInfo, String> {
    Ok(UpdateInfo {
        current_version: current_version.into(),
        latest_version: None,
        available: false,
        url: Some(RELEASES_PAGE.into()),
        status: "Windows 不在应用内查询更新；请访问发布页手动查看版本".into(),
    })
}

pub fn open(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    if url != RELEASES_PAGE {
        return Err("只允许打开固定 PureFlow 发布页".into());
    }
    app.opener()
        .open_url(RELEASES_PAGE, None::<&str>)
        .map_err(|error| format!("无法打开发布页：{error}"))
}
