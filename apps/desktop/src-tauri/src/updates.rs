// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Explicit, manual update checks. No tokens, private statistics, download, or install.
//!
//! Call `check` on a blocking worker (e.g. `tauri::async_runtime::spawn_blocking`),
//! never on the GUI thread. Only this backend calls tauri-plugin-opener; do not
//! grant the webview generic opener permissions. Initialize opener with
//! `Builder::new().open_js_links_on_click(false).build()` to avoid JS link hooks.

use reqwest::blocking::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, io::Read, time::Duration};
use tauri_plugin_opener::OpenerExt;
use url::Url;

// Gitea is mounted under /gitea: the API prefix precedes repos, not the owner.
const LATEST_API: &str = "https://115613.xyz/gitea/api/v1/repos/jvz/pure-flow/releases/latest";
const RELEASES_API: &str = "https://115613.xyz/gitea/api/v1/repos/jvz/pure-flow/releases?limit=1";
const HOST: &str = "115613.xyz";
const RELEASES_PATH: &str = "/gitea/jvz/pure-flow/releases";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_URL_BYTES: usize = 2048;

#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub available: bool,
    pub url: Option<String>,
    pub status: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Copy, Debug)]
struct Platform {
    os: &'static str,
    arch: &'static str,
}

impl Platform {
    fn for_target(os: &str, arch: &str) -> Option<Self> {
        let os = match os {
            "macos" => "macos",
            "windows" => "windows",
            _ => return None,
        };
        let arch = match arch {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            _ => return None,
        };
        Some(Self { os, arch })
    }

    fn current() -> Option<Self> {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn asset_names(self, version: &Version) -> Vec<String> {
        let mut names = vec![format!("PureFlow-{version}-{}-{}.zip", self.os, self.arch)];
        if self.os == "windows" && self.arch == "x64" {
            names.push(format!("PureFlow_{version}_x64-setup.exe"));
        }
        names
    }

    fn matches_asset(self, name: &str, version: &Version) -> bool {
        self.asset_names(version)
            .iter()
            .any(|expected| expected == name)
    }
}

fn version(tag: &str) -> Result<Version, String> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .map_err(|_| "更新版本号不是有效的 SemVer".to_owned())
}

fn stable_version(tag: &str) -> Result<Version, String> {
    let parsed = version(tag)?;
    if !parsed.pre.is_empty() {
        return Err("更新版本不是稳定版".into());
    }
    Ok(parsed)
}

/// Validate the *raw* spelling before URL parsing can normalize dot segments,
/// backslashes, escaped separators, empty credentials, or a noncanonical host.
/// Queries and fragments are unnecessary here and are deliberately forbidden.
fn trusted_url(raw: &str) -> Result<Url, String> {
    if raw.len() > MAX_URL_BYTES
        || raw.bytes().any(|byte| !byte.is_ascii_graphic())
        || raw.contains(['%', '\\', '?', '#', '@'])
    {
        return Err("更新链接包含不允许的字符".into());
    }
    let path = raw
        .strip_prefix("https://115613.xyz")
        .map(|tail| tail.strip_prefix(":443").unwrap_or(tail))
        .filter(|tail| tail.starts_with('/'))
        .ok_or("更新链接必须使用可信 HTTPS 主机和端口")?;
    if path.split('/').any(|part| part == "." || part == "..") {
        return Err("更新链接不能包含路径跳转".into());
    }
    let parsed = Url::parse(raw).map_err(|_| "更新链接无效")?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some(HOST)
        || parsed.port_or_known_default() != Some(443)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != path
    {
        return Err("更新链接不属于可信地址".into());
    }
    Ok(parsed)
}

/// Only this repository's release pages or exactly named GUI assets for this
/// platform. Generic repository paths and Gitea /attachments/{id} are NOT
/// accepted: the latter carry no repository/platform identity in their path.
fn validate_open_url(raw: &str, platform: Option<Platform>) -> Result<Url, String> {
    let parsed = trusted_url(raw)?;
    let path = parsed.path();
    if path == RELEASES_PATH {
        return Ok(parsed);
    }
    if let Some(tag) = path.strip_prefix(&format!("{RELEASES_PATH}/tag/")) {
        if tag.contains('/') {
            return Err("更新发布页面路径无效".into());
        }
        stable_version(tag)?;
        return Ok(parsed);
    }
    if let Some(tail) = path.strip_prefix(&format!("{RELEASES_PATH}/download/")) {
        let (tag, name) = tail.split_once('/').ok_or("更新资产路径无效")?;
        let latest = stable_version(tag)?;
        if platform.is_some_and(|target| target.matches_asset(name, &latest)) {
            return Ok(parsed);
        }
    }
    Err("只允许打开 PureFlow 的发布页面或对应平台的安装资产".into())
}

fn validate_release_url(raw: &str, tag: &str) -> Result<Url, String> {
    let parsed = trusted_url(raw)?;
    if parsed.path() != format!("{RELEASES_PATH}/tag/{tag}") {
        return Err("发布页面与更新版本不匹配".into());
    }
    Ok(parsed)
}

fn read_limited(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    reader
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|error| format!("无法读取更新响应：{error}"))?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err("更新响应超过 1 MiB 限制".into());
    }
    Ok(body)
}

fn release_info(
    current_version: &str,
    body: &[u8],
    platform: Option<Platform>,
) -> Result<UpdateInfo, String> {
    let current = version(current_version)?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err("更新响应超过 1 MiB 限制".into());
    }
    let release: Release =
        serde_json::from_slice(body).map_err(|_| "更新响应不是有效的发布 JSON")?;
    let mut info = UpdateInfo {
        current_version: current_version.into(),
        latest_version: None,
        available: false,
        url: None,
        status: "没有可用的稳定版本".into(),
    };
    if release.draft || release.prerelease {
        return Ok(info);
    }
    let latest = stable_version(&release.tag_name)?;
    let page = validate_release_url(&release.html_url, &release.tag_name)?;
    info.latest_version = Some(latest.to_string());
    // cmp_precedence ignores build metadata and handles numeric components and
    // prereleases correctly: 1.10.0 > 1.9.0, 1.0.0 > 1.0.0-rc.1.
    if latest.cmp_precedence(&current) != Ordering::Greater {
        info.status = "当前已是最新版本".into();
        return Ok(info);
    }
    let Some(platform) = platform else {
        info.status = "新版本暂未提供本机平台的安装包".into();
        return Ok(info);
    };
    // Require an exactly named PureFlow GUI asset for this platform and version;
    // unrelated or historical packages cannot qualify.
    let Some(asset) = release
        .assets
        .iter()
        .find(|asset| platform.matches_asset(&asset.name, &latest))
    else {
        info.status = "新版本暂未提供本机平台的安装包".into();
        return Ok(info);
    };
    info.available = true;
    info.status = "发现新版本，可手动打开下载页面".into();
    // Prefer a corresponding direct download URL only if its full path binds
    // both version and asset name. Otherwise use the trusted release page,
    // notably for Gitea's opaque attachment URLs; never open those blindly.
    let direct = trusted_url(&asset.browser_download_url).ok().filter(|url| {
        url.path()
            == format!(
                "{RELEASES_PATH}/download/{}/{}",
                release.tag_name, asset.name
            )
    });
    info.url = Some(direct.unwrap_or(page).into());
    Ok(info)
}

fn request_error(error: reqwest::Error) -> String {
    let category = if error.is_timeout() {
        "超时"
    } else if error.is_connect() {
        "连接或 TLS 失败"
    } else {
        "请求失败"
    };
    let mut details = Vec::new();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        details.push(cause.to_string());
        if details.len() == 4 {
            break;
        }
        source = cause.source();
    }
    // The only URL is the fixed public release API; no token or local identifier.
    format!(
        "更新检查{category}：{}{}",
        error.without_url(),
        if details.is_empty() {
            String::new()
        } else {
            format!("；{}", details.join("；"))
        }
    )
}
fn fetch_body(client: &Client, endpoint: &str) -> Result<(u16, Vec<u8>), String> {
    let response = client
        .get(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(request_error)?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok());
        let downgrade = location
            .and_then(|value| Url::parse(value).ok())
            .is_some_and(|url| url.scheme() != "https");
        return Err(if downgrade {
            "更新服务尝试重定向至非 HTTPS 地址，已拒绝；请检查服务端反向代理/访问策略".into()
        } else {
            format!("更新服务返回 HTTP {status} 重定向，客户端不会自动跟随；请让公开更新 API 直接返回 JSON")
        });
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err("更新响应超过 1 MiB 限制".into());
    }
    Ok((status, read_limited(response)?))
}
fn missing_latest(current: &str, status: u16, body: &[u8]) -> Result<UpdateInfo, String> {
    if status != 200 {
        return Err(format!(
            "更新发布列表返回 HTTP {status}；不能确认仓库是否尚未发布"
        ));
    }
    let releases: Vec<serde_json::Value> =
        serde_json::from_slice(body).map_err(|_| "发布列表响应不是有效 JSON 数组")?;
    if !releases.is_empty() {
        return Err("仓库存在发布但没有可用的最新稳定版，请检查发布标记和更新端点".into());
    }
    Ok(UpdateInfo {
        current_version: current.into(),
        latest_version: None,
        available: false,
        url: None,
        status: "此仓库尚未发布版本；当前不能比较更新".into(),
    })
}

/// Public, fixed-endpoint request only. No authentication, system identifiers,
/// private sampling data, cookies, or statistics are sent.
pub fn check(current_version: &str) -> Result<UpdateInfo, String> {
    version(current_version)?;
    let client = Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .timeout(Duration::from_secs(10))
        // Deny all redirects, a stricter policy than same HTTPS host/path.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("无法创建更新请求：{error}"))?;
    let (status, body) = fetch_body(&client, LATEST_API)?;
    if status == 404 {
        // A 404 alone can also mean a wrong endpoint/hidden repository. Confirm
        // the public list exists and is empty instead of claiming 'up to date'.
        let (list_status, list_body) = fetch_body(&client, RELEASES_API)?;
        return missing_latest(current_version, list_status, &list_body);
    }
    if !(200..300).contains(&status) {
        return Err(format!("更新服务返回 HTTP {status}"));
    }
    release_info(current_version, &body, Platform::current())
}

/// Revalidate even when called directly via IPC; the UI cannot launch arbitrary
/// protocols, hosts, local files, generic Gitea paths, or another platform's file.
pub fn open(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    let trusted = validate_open_url(url, Platform::current())?;
    app.opener()
        .open_url(trusted.as_str(), None::<&str>)
        .map_err(|error| format!("无法打开更新页面：{error}"))
}
