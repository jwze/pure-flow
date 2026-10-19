// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod cache_fs;
mod export;
#[cfg(windows)]
#[allow(dead_code)] // Fixed transport also serves the separately compiled helper.
mod helper;
mod history_writer;
mod model;
mod network_permission;
mod panel_state;
mod placement;
#[cfg(windows)]
mod popup_focus;
mod preferences;
mod runtime;
mod sampling_policy;
mod shell;
mod shell_diagnostics;
mod singleton;
mod storage_paths;
#[cfg(target_os = "macos")]
mod tray_macos;
mod tray_readout;
#[cfg(not(windows))]
mod updates;
#[cfg(windows)]
#[path = "updates_windows.rs"]
mod updates;
use runtime::Monitor;
#[cfg(windows)]
static EARLY_SHOW: AtomicBool = AtomicBool::new(false);
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};
use tauri::{Manager, State};

#[derive(Default)]
struct ExitState {
    closing: AtomicBool,
    cleaned: AtomicBool,
}
struct TrayWorker(Mutex<Option<shell::Updater>>);
#[cfg(target_os = "macos")]
struct SingletonOwner(Mutex<Option<singleton::Singleton>>);

#[tauri::command]
async fn get_history(
    app: tauri::AppHandle,
    query: pureflow_storage::HistoryQuery,
) -> Result<pureflow_storage::HistoryResult, String> {
    let hold = shell::hold_panel(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _hold = hold;
        app.state::<Monitor>().history(query)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn cancel_history(monitor: State<'_, Monitor>) {
    monitor.cancel_history();
}
#[tauri::command]
async fn export_history(
    app: tauri::AppHandle,
    query: pureflow_storage::HistoryQuery,
) -> Result<Option<String>, String> {
    let hold = shell::hold_panel(&app)?;
    let worker_app = app.clone();
    let csv = tauri::async_runtime::spawn_blocking(move || {
        worker_app.state::<Monitor>().export_csv(query)
    })
    .await
    .map_err(|e| e.to_string())??;
    tauri::async_runtime::spawn_blocking(move || {
        let _hold = hold;
        export::save_csv(&csv)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn get_preferences(app: tauri::AppHandle) -> Result<preferences::Preferences, String> {
    preferences::get(&app)
}
#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<preferences::Preferences, String> {
    let _hold = shell::hold_panel(&app)?;
    preferences::set(&app, enabled)
}
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<updates::UpdateInfo, String> {
    let hold = shell::hold_panel(&app)?;
    let version = app.package_info().version.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let _hold = hold;
        updates::check(&version)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn request_network_permission(app: tauri::AppHandle) -> Result<String, String> {
    let hold = shell::hold_panel(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _hold = hold;
        network_permission::request(&app)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn open_update(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let _hold = shell::hold_panel(&app)?;
    updates::open(&app, &url)
}
#[tauri::command]
fn get_snapshot(
    monitor: State<'_, Monitor>,
    known_revision: Option<String>,
) -> Result<runtime::SnapshotResponse, String> {
    monitor.snapshot(known_revision)
}
#[tauri::command]
fn set_paused(paused: bool, monitor: State<'_, Monitor>) -> Result<(), String> {
    monitor.pause(paused)
}
#[tauri::command]
async fn retry_sampling(app: tauri::AppHandle) -> Result<(), String> {
    let hold = shell::hold_panel(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _hold = hold;
        app.state::<Monitor>().retry()
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0)
}
#[tauri::command]
async fn hide_panel(app: tauri::AppHandle) -> Result<(), String> {
    app.state::<Monitor>().cancel_history();
    shell::hide_panel(&app).map_err(|e| e.to_string())
}
#[tauri::command]
async fn get_shell_state(app: tauri::AppHandle) -> shell::ShellState {
    shell::state(&app)
}
#[tauri::command]
fn take_about_navigation(app: tauri::AppHandle) -> Result<bool, String> {
    shell::take_about_navigation(&app)
}
#[tauri::command]
fn get_panel_state(
    memory: State<'_, panel_state::Memory>,
) -> Result<panel_state::PanelState, String> {
    memory.get()
}
#[tauri::command]
fn save_panel_state(
    state: panel_state::PanelState,
    memory: State<'_, panel_state::Memory>,
) -> Result<(), String> {
    memory.save(state)
}
#[tauri::command]
async fn get_storage_status(app: tauri::AppHandle) -> Result<storage_paths::StorageStatus, String> {
    let hold = shell::hold_panel(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _hold = hold;
        storage_paths::status(&app)
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
fn clear_cache(app: tauri::AppHandle) -> Result<String, String> {
    shell::request_cache_cleanup(&app)
}
#[tauri::command]
async fn compact_history(app: tauri::AppHandle) -> Result<String, String> {
    let hold = shell::hold_panel(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _hold = hold;
        app.state::<Monitor>().compact_history()
    })
    .await
    .map_err(|e| e.to_string())?
}
fn main() {
    #[cfg(windows)]
    if let Err(error) = helper::ensure_gui_not_elevated() {
        helper::show_startup_error(&error);
        std::process::exit(1);
    }
    #[cfg(target_os = "macos")]
    let singleton = match singleton::Singleton::claim() {
        Ok(Some(owner)) => owner,
        Ok(None) => return,
        Err(error) => {
            eprintln!("净流启动失败：{error}");
            return;
        }
    };
    let builder = tauri::Builder::default();
    #[cfg(windows)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| {
        if app.try_state::<TrayWorker>().is_none() {
            EARLY_SHOW.store(true, Ordering::Release);
            return;
        }
        let _ = shell::show_panel(app);
    }));
    #[cfg(target_os = "macos")]
    let builder = builder.manage(SingletonOwner(Mutex::new(Some(singleton))));
    builder
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .manage(ExitState::default())
        .manage(panel_state::Memory::default())
        .setup(|app| {
            #[cfg(windows)]
            app.manage(singleton::CollectorOwnership::claim().map_err(std::io::Error::other)?);
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                app.set_dock_visibility(false);
            }
            #[cfg(not(windows))]
            let data_dir = app.path().app_data_dir()?;
            #[cfg(windows)]
            let data_dir = storage_paths::database_path(app.handle())
                .map_err(std::io::Error::other)?
                .parent()
                .ok_or_else(|| std::io::Error::other("数据库父目录缺失"))?
                .to_path_buf();
            std::fs::create_dir_all(&data_dir)?;
            // Release the fixed, ephemeral WebView cache only before any WebView exists.
            if let Err(error) = storage_paths::clear_webview_cache(app.handle()) {
                eprintln!("缓存清理稍后重试：{error}");
            }
            app.manage(
                Monitor::start(&data_dir.join("pureflow.sqlite3"))
                    .map_err(std::io::Error::other)?,
            );
            app.manage(TrayWorker(Mutex::new(Some(shell::install(app.handle())?))));
            #[cfg(windows)]
            if EARLY_SHOW.swap(false, Ordering::AcqRel) {
                let _ = shell::show_panel(app.handle());
            }
            #[cfg(target_os = "macos")]
            if let Some(owner) = app
                .state::<SingletonOwner>()
                .0
                .lock()
                .map_err(|_| std::io::Error::other("单实例状态不可用"))?
                .as_mut()
            {
                owner
                    .attach(app.handle().clone())
                    .map_err(std::io::Error::other)?;
            }
            let handle = app.handle().clone();
            ctrlc::set_handler(move || handle.exit(0))?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_history,
            cancel_history,
            export_history,
            get_preferences,
            set_autostart,
            check_update,
            request_network_permission,
            open_update,
            get_snapshot,
            set_paused,
            retry_sampling,
            quit_app,
            hide_panel,
            get_shell_state,
            take_about_navigation,
            get_panel_state,
            save_panel_state,
            get_storage_status,
            clear_cache,
            compact_history
        ])
        .build(tauri::generate_context!())
        .expect("cannot build PureFlow")
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                let exit = app.state::<ExitState>();
                if exit.cleaned.load(Ordering::Acquire) {
                    return;
                }
                api.prevent_exit();
                if code.is_none() {
                    return;
                }
                if !exit.closing.swap(true, Ordering::AcqRel) {
                    let handle = app.clone();
                    std::thread::spawn(move || {
                        if let Ok(mut worker) = handle.state::<TrayWorker>().0.lock() {
                            if let Some(mut updater) = worker.take() {
                                updater.stop();
                            }
                        }
                        handle.state::<Monitor>().shutdown();
                        #[cfg(target_os = "macos")]
                        if let Ok(mut owner) = handle.state::<SingletonOwner>().0.lock() {
                            owner.take();
                        }
                        handle
                            .state::<ExitState>()
                            .cleaned
                            .store(true, Ordering::Release);
                        handle.exit(code.unwrap_or(0));
                    });
                }
            }
        });
}
