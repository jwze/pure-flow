// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! One event-driven tray/deadline worker; native window operations are queued on the GUI thread.
use crate::{
    placement::{self, Area},
    runtime::Monitor,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewWindow, WebviewWindowBuilder,
};

const TRAY_ID: &str = "pureflow-tray";
const RELEASE_AFTER: Duration = Duration::from_secs(60);
#[derive(Default)]
struct Life {
    generation: u64,
    hidden_since: Option<Instant>,
    holds: usize,
    release_queued: bool,
    clear_pending: bool,
    cleaning: bool,
    destroying: bool,
    destroyed_ready: bool,
    cleanup_ready: bool,
    #[cfg(windows)]
    cleanup_after: Option<Instant>,
    reopen: bool,
    cache_error: Option<String>,
    panel_error: Option<String>,
    about_pending: bool,
    #[cfg(not(windows))]
    last_blur: Option<Instant>,
    #[cfg(windows)]
    focus: crate::popup_focus::PopupFocus,
}
struct Lifecycle {
    inner: Mutex<Life>,
    wake: SyncSender<()>,
}
impl Lifecycle {
    fn wake(&self) {
        let _ = self.wake.try_send(());
    }
}
pub struct NativeOperationGuard {
    life: Arc<Lifecycle>,
}
impl Drop for NativeOperationGuard {
    fn drop(&mut self) {
        let mut state = self.life.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.holds = state.holds.saturating_sub(1);
        drop(state);
        self.life.wake();
    }
}
pub fn hold_panel(app: &tauri::AppHandle) -> Result<NativeOperationGuard, String> {
    let life = app.state::<Arc<Lifecycle>>().inner().clone();
    let mut state = life.inner.lock().map_err(|_| "弹层状态不可用")?;
    if state.destroying || state.cleaning {
        return Err("界面正在释放，请重新打开后重试".into());
    }
    state.holds += 1;
    drop(state);
    Ok(NativeOperationGuard { life })
}
#[derive(serde::Serialize)]
pub struct ShellState {
    pub tray_exists: bool,
    pub panel_created: bool,
    pub panel_visible: bool,
    pub panel_focused: bool,
    pub cache_error: Option<String>,
    pub panel_error: Option<String>,
}
pub fn state(app: &tauri::AppHandle) -> ShellState {
    let panel = app.get_webview_window("main");
    let cache_error = app.try_state::<Arc<Lifecycle>>().and_then(|life| {
        life.inner
            .lock()
            .ok()
            .and_then(|state| state.cache_error.clone())
    });
    let panel_error = app.try_state::<Arc<Lifecycle>>().and_then(|life| {
        life.inner
            .lock()
            .ok()
            .and_then(|state| state.panel_error.clone())
    });
    ShellState {
        panel_error,
        tray_exists: app.tray_by_id(TRAY_ID).is_some(),
        panel_created: panel.is_some(),
        panel_visible: panel
            .as_ref()
            .is_some_and(|w| w.is_visible().unwrap_or(false)),
        panel_focused: panel.is_some_and(|w| w.is_focused().unwrap_or(false)),
        cache_error,
    }
}
pub struct Updater {
    stop: Arc<AtomicBool>,
    wake: SyncSender<()>,
    worker: Option<JoinHandle<()>>,
}
impl Updater {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Updater {
    fn drop(&mut self) {
        self.stop();
    }
}
#[cfg(not(target_os = "macos"))]
fn tray_image() -> tauri::image::Image<'static> {
    let mut rgba = vec![0u8; 24 * 24 * 4];
    for (start, top) in [(3, 14), (10, 9), (17, 4)] {
        for y in top..21 {
            for x in start..start + 4 {
                let offset = (y * 24 + x) * 4;
                #[cfg(not(target_os = "macos"))]
                rgba[offset..offset + 3].copy_from_slice(&[46, 189, 135]);
                rgba[offset + 3] = 255;
            }
        }
    }
    tauri::image::Image::new_owned(rgba, 24, 24)
}
fn initial_tray_image(app: &tauri::AppHandle) -> tauri::image::Image<'static> {
    #[cfg(target_os = "macos")]
    {
        match crate::tray_macos::render(&["—".into(), "—".into()]) {
            Ok(image) => image,
            Err(error) => {
                // No previous image at startup: retain the original compact mark, never
                // fall back to a side-by-side title. Later state updates can retry drawing.
                app.state::<Arc<Lifecycle>>()
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .panel_error = Some(format!("菜单栏绘制失败：{error}"));
                crate::tray_macos::fallback()
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        tray_image()
    }
}

#[derive(Clone, PartialEq)]
struct TrayUpdate {
    #[cfg(target_os = "macos")]
    lines: [String; 2],
    tooltip: String,
    paused: bool,
}
#[derive(Default)]
struct PendingTray {
    #[cfg(target_os = "macos")]
    rendered: Option<[String; 2]>,
    latest: Option<TrayUpdate>,
    scheduled: bool,
}

pub fn install(app: &tauri::AppHandle) -> tauri::Result<Updater> {
    app.manage(crate::shell_diagnostics::Journal::new(
        app.path()
            .app_local_data_dir()
            .ok()
            .map(|root| root.join("logs").join("popup.log")),
    ));
    journal(
        app,
        &format!("shell-start version={}", app.package_info().version),
    );
    let (wake, events) = app.state::<Monitor>().subscribe();
    let life = Arc::new(Lifecycle {
        #[cfg(not(windows))]
        inner: Mutex::new(Life::default()),
        #[cfg(windows)]
        inner: Mutex::new(Life {
            // main's early pass reports to stderr. One bounded retry here, still
            // before any window exists, retains failures for the GUI diagnostics.
            cache_error: crate::storage_paths::clear_webview_cache(app).err(),
            ..Life::default()
        }),
        wake: wake.clone(),
    });
    app.manage(life.clone());
    let show = MenuItem::with_id(app, "show", "查看统计", true, None::<&str>)?;
    let about = MenuItem::with_id(
        app,
        "about",
        "关于 PureFlow · Developed by jvz",
        true,
        None::<&str>,
    )?;
    let pause = CheckMenuItem::with_id(app, "pause", "暂停记录", true, false, None::<&str>)?;
    let retry = MenuItem::with_id(app, "retry", "重新采集", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 净流 PureFlow", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &pause, &retry, &about, &separator, &quit])?;
    let builder = TrayIconBuilder::with_id(TRAY_ID)
        .icon(initial_tray_image(app))
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("净流 PureFlow · 正在准备采集")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                let _ = show_panel(app);
            }
            "about" => {
                app.state::<Arc<Lifecycle>>()
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .about_pending = true;
                let _ = show_panel(app);
            }
            "pause" => {
                let monitor = app.state::<Monitor>();
                if let Ok(status) = monitor.tray_status() {
                    let _ = monitor.pause(!status.paused);
                }
            }
            "retry" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    let _ = app.state::<Monitor>().retry();
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            #[cfg(windows)]
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Down,
                ..
            } => {
                let app = tray.app_handle();
                app.state::<Arc<Lifecycle>>()
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .focus
                    .tray_down();
                journal(app, "tray-left-down");
            }
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => {
                let app = tray.app_handle();
                #[cfg(windows)]
                {
                    let show = app
                        .state::<Arc<Lifecycle>>()
                        .inner
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .focus
                        .tray_up_should_show();
                    journal(
                        app,
                        if show {
                            "tray-left-up show"
                        } else {
                            "tray-left-up hide"
                        },
                    );
                    if show {
                        let _ = show_panel(app);
                    } else {
                        let _ = hide_panel(app);
                    }
                }
                #[cfg(not(windows))]
                {
                    let blurred = app
                        .state::<Arc<Lifecycle>>()
                        .inner
                        .lock()
                        .ok()
                        .and_then(|s| s.last_blur)
                        .is_some_and(|last| last.elapsed() < Duration::from_millis(300));
                    if blurred {
                        let _ = hide_panel(app);
                    } else {
                        let _ = toggle_panel(app);
                    }
                }
            }
            TrayIconEvent::Click {
                button: MouseButton::Right,
                button_state: MouseButtonState::Down,
                ..
            } => {
                let _ = hide_panel(tray.app_handle());
            }
            _ => {}
        });
    #[cfg(target_os = "macos")]
    let builder = builder.title("");
    let tray = builder.build(app)?;
    let stop = Arc::new(AtomicBool::new(false));
    let closing = stop.clone();
    let handle = app.clone();
    let worker = thread::Builder::new()
        .name("pureflow-shell".into())
        .spawn(move || {
            run_worker(handle, life, closing, events, tray, pause);
        })
        .map_err(tauri::Error::Io)?;
    Ok(Updater {
        stop,
        wake,
        worker: Some(worker),
    })
}
fn run_worker(
    app: tauri::AppHandle,
    life: Arc<Lifecycle>,
    stop: Arc<AtomicBool>,
    events: Receiver<()>,
    tray: tauri::tray::TrayIcon,
    pause: CheckMenuItem<tauri::Wry>,
) {
    let pending_tray = Arc::new(Mutex::new(PendingTray::default()));
    let mut previous = None;
    let mut previous_revision = None;
    while !stop.load(Ordering::Acquire) {
        #[cfg(windows)]
        {
            let revision = life
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .focus
                .take_due(Instant::now());
            if let Some(revision) = revision {
                let handle = app.clone();
                let closing = stop.clone();
                if let Err(error) = app.run_on_main_thread(move || {
                    if !closing.load(Ordering::Acquire) {
                        confirm_blur(&handle, revision);
                    }
                }) {
                    journal(&app, &format!("focus-check-queue-error {error}"));
                }
            }
        }
        let destroyed = {
            let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut state.destroyed_ready)
        };
        if destroyed {
            let handle = app.clone();
            let closing = stop.clone();
            let _ = app.run_on_main_thread(move || {
                if !closing.load(Ordering::Acquire) {
                    finish_release(&handle);
                }
            });
        }
        let cleanup = {
            let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
            #[cfg(windows)]
            {
                if state.holds != 0 || state.cleanup_after.is_some_and(|at| at > Instant::now()) {
                    // Guard drop wakes the worker; never spin or consume this request.
                    false
                } else {
                    state.cleanup_after = None;
                    std::mem::take(&mut state.cleanup_ready)
                }
            }
            #[cfg(not(windows))]
            std::mem::take(&mut state.cleanup_ready)
        };
        if cleanup {
            let result = crate::storage_paths::clear_webview_cache(&app);
            let reopen = {
                let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
                state.cache_error = result.err();
                state.cleaning = false;
                std::mem::take(&mut state.reopen)
            };
            if reopen && !stop.load(Ordering::Acquire) {
                let _ = show_panel(&app);
            }
        }
        if let Ok(status) = app.state::<Monitor>().tray_status() {
            let lines =
                crate::tray_readout::lines(&status.phase, status.upload_bps, status.download_bps);
            let tooltip = crate::tray_readout::tooltip(&lines, &status.status);
            let next = TrayUpdate {
                #[cfg(target_os = "macos")]
                lines,
                tooltip,
                paused: status.paused,
            };
            if previous.as_ref() != Some(&next) || {
                let state = pending_tray.lock().unwrap_or_else(|e| e.into_inner());
                state.latest.is_some() && !state.scheduled
            } {
                previous = Some(next.clone());
                queue_tray_update(&app, &life, &stop, &pending_tray, &tray, &pause, next);
            }
        }
        if let Ok(revision) = app.state::<Monitor>().revision() {
            if previous_revision.as_ref() != Some(&revision) {
                previous_revision = Some(revision.clone());
                if let Some(window) = app.get_webview_window("main") {
                    if window.is_visible().unwrap_or(false) {
                        let _ = window.emit("snapshot-updated", revision);
                    }
                }
            }
        }
        let (deadline, release_deadline) = {
            let state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
            #[cfg(windows)]
            let cleanup_deadline = if state.cleanup_ready && state.holds == 0 {
                state.cleanup_after
            } else {
                None
            };
            #[cfg(not(windows))]
            let cleanup_deadline: Option<Instant> = None;
            let release_deadline = cleanup_deadline.or_else(|| {
                if state.holds == 0 && !state.release_queued && !state.cleaning && !state.destroying
                {
                    state.hidden_since.map(|hidden| hidden + RELEASE_AFTER)
                } else {
                    None
                }
            });
            #[cfg(windows)]
            let deadline = state
                .focus
                .deadline()
                .into_iter()
                .chain(release_deadline)
                .min();
            #[cfg(not(windows))]
            let deadline = release_deadline;
            (deadline, release_deadline)
        };
        if release_deadline.is_some_and(|at| at <= Instant::now()) {
            #[cfg(windows)]
            {
                let state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
                if state.cleanup_ready {
                    // Re-enter the cleanup branch, not the destroy branch.
                    continue;
                }
            }
            queue_release(&app, &life, &stop);
            continue;
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        let received = if let Some(at) = deadline {
            events
                .recv_timeout(at.saturating_duration_since(Instant::now()))
                .map_err(|e| matches!(e, std::sync::mpsc::RecvTimeoutError::Disconnected))
        } else {
            events.recv().map_err(|_| true)
        };
        if received == Err(true) {
            break;
        }
    }
}
fn queue_tray_update(
    app: &tauri::AppHandle,
    life: &Arc<Lifecycle>,
    stop: &Arc<AtomicBool>,
    pending: &Arc<Mutex<PendingTray>>,
    tray: &tauri::tray::TrayIcon,
    pause: &CheckMenuItem<tauri::Wry>,
    next: TrayUpdate,
) {
    let mut state = pending.lock().unwrap_or_else(|e| e.into_inner());
    state.latest = Some(next);
    if state.scheduled {
        return;
    }
    state.scheduled = true;
    drop(state);
    let pending = pending.clone();
    let fallback = pending.clone();
    let tray = tray.clone();
    let pause = pause.clone();
    let stop = stop.clone();
    let life = life.clone();
    #[cfg(target_os = "macos")]
    let handle = app.clone();
    if app
        .run_on_main_thread(move || {
            // Keep scheduled=true while drawing. The worker can only replace one latest value.
            let next = pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .latest
                .take();
            if let Some(next) = next.filter(|_| !stop.load(Ordering::Acquire)) {
                #[cfg(target_os = "macos")]
                let mut tooltip = next.tooltip.clone();
                #[cfg(not(target_os = "macos"))]
                let tooltip = next.tooltip.clone();
                #[cfg(target_os = "macos")]
                {
                    let redraw = pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rendered
                        .as_ref()
                        != Some(&next.lines);
                    if redraw {
                        let result = crate::tray_macos::render(&next.lines).and_then(|image| {
                            tray.set_icon(Some(image)).map_err(|e| e.to_string())?;
                            // set_icon resets template; restore it on every replacement.
                            tray.set_icon_as_template(true).map_err(|e| e.to_string())?;
                            tray.set_title(Some("")).map_err(|e| e.to_string())
                        });
                        match result {
                            Ok(()) => {
                                pending.lock().unwrap_or_else(|e| e.into_inner()).rendered =
                                    Some(next.lines.clone())
                            }
                            Err(error) => {
                                let message = format!("菜单栏绘制失败（保留原图像）：{error}");
                                life.inner
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .panel_error = Some(message.clone());
                                let _ = handle.emit("panel-error", &message);
                                tooltip = format!("{}\n{message}", next.tooltip);
                            }
                        }
                    }
                }
                let _ = tray.set_tooltip(Some(&tooltip));
                let _ = pause.set_checked(next.paused);
            }
            let mut state = pending.lock().unwrap_or_else(|e| e.into_inner());
            state.scheduled = false;
            // An update arriving during drawing is handed back to the worker, not another GUI loop.
            if state.latest.is_some() {
                life.wake();
            }
        })
        .is_err()
    {
        fallback.lock().unwrap_or_else(|e| e.into_inner()).scheduled = false;
    }
}

fn queue_release(app: &tauri::AppHandle, life: &Arc<Lifecycle>, stop: &Arc<AtomicBool>) {
    let generation = {
        let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.release_queued = true;
        state.generation
    };
    let handle = app.clone();
    let life = life.clone();
    let closing = stop.clone();
    let fallback = life.clone();
    if app
        .run_on_main_thread(move || {
            let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
            state.release_queued = false;
            if closing.load(Ordering::Acquire)
                || generation != state.generation
                || state.holds != 0
                || state.hidden_since.is_none()
            {
                drop(state);
                life.wake();
                return;
            }
            // Reserve ownership before sending Destroy: it is asynchronous even on GUI thread.
            state.destroying = true;
            drop(state);
            let result = if let Some(window) = handle.get_webview_window("main") {
                if window.is_visible().unwrap_or(true) {
                    Err("弹层仍可见".to_string())
                } else {
                    window.destroy().map_err(|e| e.to_string())
                }
            } else {
                life.inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .destroyed_ready = true;
                Ok(())
            };
            let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(()) => {
                    state.hidden_since = None;
                }
                Err(error) => {
                    state.destroying = false;
                    state.cleaning = false;
                    state.cache_error = Some(error);
                    state.hidden_since = None;
                }
            }
            drop(state);
            life.wake();
        })
        .is_err()
    {
        let mut state = fallback.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.release_queued = false;
        state.hidden_since = None;
        state.cache_error = Some("无法安排界面释放，请重新打开后重试".into());
    }
}
pub fn request_cache_cleanup(app: &tauri::AppHandle) -> Result<String, String> {
    let life = app.state::<Arc<Lifecycle>>();
    let mut state = life.inner.lock().map_err(|_| "弹层状态不可用")?;
    if state.holds != 0 || state.cleaning || state.destroying {
        return Err("原生操作进行中，请稍后清理缓存".into());
    }
    state.clear_pending = true;
    state.cache_error = None;
    drop(state);
    life.wake();
    Ok("已安排清理：弹层连续收起 60 秒后释放界面并清理缓存；重新打开会延后清理，历史记录不受影响。".into())
}
fn journal(app: &tauri::AppHandle, event: &str) {
    if let Some(log) = app.try_state::<crate::shell_diagnostics::Journal>() {
        log.record(event);
    }
}

#[cfg(windows)]
fn confirm_blur(app: &tauri::AppHandle, revision: u64) {
    let life = app.state::<Arc<Lifecycle>>();
    if !life
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .focus
        .is_current(revision)
    {
        journal(app, "focus-check-cancelled");
        return;
    }
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let foreground = crate::shell_diagnostics::foreground(&window);
    journal(
        app,
        &format!(
            "focus-check panel_foreground={} foreground_class={}",
            foreground.panel, foreground.class
        ),
    );
    // Native foreground ownership is authoritative. WebView2's queued LostFocus
    // can be stale even when the HWND (or one of its children) is already active.
    if foreground.panel {
        life.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .focus
            .confirmed();
    } else {
        let retry = life
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .focus
            .retry_shell_activation(foreground.shell, Instant::now());
        if retry {
            journal(app, "focus-retry-shell-activation");
            report_native(app, window.set_focus());
            life.wake();
        } else {
            journal(app, "hide-confirmed-blur");
            report_native(app, hide_native(app));
        }
    }
}

fn panel(app: &tauri::AppHandle) -> tauri::Result<WebviewWindow> {
    // Only called on GUI thread: no creation mutex can deadlock WebView creation.
    if let Some(window) = app.get_webview_window("main") {
        return Ok(window);
    }
    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|config| config.label == "main")
        .ok_or_else(|| tauri::Error::AssetNotFound("popup configuration".into()))?;
    let builder = WebviewWindowBuilder::from_config(app, config)?;
    #[cfg(any(target_os = "macos", windows))]
    let builder = builder.incognito(true);
    #[cfg(windows)]
    let builder = builder.data_directory(app.path().app_cache_dir()?.join("webview"));
    journal(app, "panel-create-begin");
    let window = builder.build()?;
    let handle = app.clone();
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Focused(focused) => {
            journal(&handle, &format!("focus-event focused={focused}"));
            #[cfg(windows)]
            {
                let life = handle.state::<Arc<Lifecycle>>();
                let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
                if *focused {
                    state.focus.focused();
                } else {
                    state.focus.blurred(Instant::now());
                }
                drop(state);
                life.wake();
            }
            #[cfg(not(windows))]
            if !focused {
                handle
                    .state::<Arc<Lifecycle>>()
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .last_blur = Some(Instant::now());
                let _ = hide_panel(&handle);
            }
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = hide_panel(&handle);
        }
        tauri::WindowEvent::Destroyed => {
            // run_on_main_thread executes inline when already on GUI thread. Let
            // the owned worker queue the next turn, after runtime drops context.
            let life = handle.state::<Arc<Lifecycle>>();
            life.inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .destroyed_ready = true;
            life.wake();
        }
        _ => {}
    });
    journal(app, "panel-create-end");
    Ok(window)
}
fn show_native(app: &tauri::AppHandle) -> tauri::Result<()> {
    let life = app.state::<Arc<Lifecycle>>();
    {
        let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.generation = state.generation.wrapping_add(1);
        state.hidden_since = None;
        if state.cleaning || state.destroying {
            state.reopen = true;
            return Ok(());
        }
    }
    life.wake();
    let window = panel(app)?;
    let scale = window.scale_factor()?;
    let rect = app
        .tray_by_id(TRAY_ID)
        .and_then(|tray| tray.rect().ok().flatten());
    let monitor = if let Some(rect) = rect {
        let position = rect.position.to_physical::<f64>(scale);
        app.monitor_from_point(position.x, position.y)?
            .or(app.primary_monitor()?)
    } else {
        app.primary_monitor()?
    };
    if let Some(monitor) = monitor {
        let scale = monitor.scale_factor();
        let work = monitor.work_area();
        let work = Area {
            x: work.position.x as f64,
            y: work.position.y as f64,
            width: work.size.width as f64,
            height: work.size.height as f64,
        };
        let anchor = if let Some(rect) = rect {
            let position = rect.position.to_physical::<f64>(scale);
            let size = rect.size.to_physical::<f64>(scale);
            Area {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            }
        } else {
            Area {
                x: work.x + work.width - 24.0 * scale,
                y: work.y,
                width: 0.0,
                height: 0.0,
            }
        };
        let result = placement::popup(anchor, work, scale);
        window.set_size(PhysicalSize::new(result.width, result.height))?;
        window.set_position(PhysicalPosition::new(result.x, result.y))?;
    }
    journal(app, "show-begin");
    #[cfg(windows)]
    life.inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .focus
        .shown(Instant::now());
    window.show()?;
    window.set_focus()?;
    life.wake();
    journal(app, "show-requested");
    #[cfg(target_os = "macos")]
    if let Some(marker) = objc2::MainThreadMarker::new() {
        let application = objc2_app_kit::NSApplication::sharedApplication(marker);
        #[allow(deprecated)]
        application.activateIgnoringOtherApps(true);
        if let Ok(pointer) = window.ns_window() {
            // SAFETY: Tauri owns this live NSWindow; this function runs on the GUI thread.
            let native = unsafe { &*pointer.cast::<objc2_app_kit::NSWindow>() };
            native.makeKeyAndOrderFront(None);
            native.makeKeyWindow();
        }
    }
    window.emit("panel-visibility", true)?;
    // Keep the intent until the frontend consumes it, including a newly created WebView.
    if life
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .about_pending
    {
        window.emit("panel-about", ())?;
    }
    Ok(())
}
fn hide_native(app: &tauri::AppHandle) -> tauri::Result<()> {
    journal(app, "hide-requested");
    #[cfg(windows)]
    app.state::<Arc<Lifecycle>>()
        .inner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .focus
        .hidden();
    if let Some(window) = app.get_webview_window("main") {
        window.emit("panel-visibility", false)?;
        window.hide()?;
        let life = app.state::<Arc<Lifecycle>>();
        let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.hidden_since.is_none() {
            state.hidden_since = Some(Instant::now());
            state.generation = state.generation.wrapping_add(1);
        }
        drop(state);
        life.wake();
    }
    Ok(())
}
fn finish_release(app: &tauri::AppHandle) {
    let life = app.state::<Arc<Lifecycle>>();
    let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
    state.destroying = false;
    state.hidden_since = None;
    let clear = std::mem::take(&mut state.clear_pending);
    // Windows owns a fixed ephemeral profile. Destroyed is handed through the
    // worker to this next GUI turn, so the runtime has dropped its WebView context
    // before cleanup is allowed. Keep cleaning/reopen reservation until it ends.
    #[cfg(windows)]
    let clear = {
        let _ = clear;
        true
    };
    state.cleaning = clear;
    state.cleanup_ready = clear;
    #[cfg(windows)]
    {
        // Destroyed/context release precedes asynchronous WebView2 process exit.
        // Keep the profile reserved through one bounded engine grace period.
        state.cleanup_after = clear.then(|| Instant::now() + Duration::from_secs(3));
    }
    let reopen = if clear {
        false
    } else {
        std::mem::take(&mut state.reopen)
    };
    drop(state);
    life.wake();
    if reopen {
        report_native(app, show_native(app));
    }
}
fn report_native(app: &tauri::AppHandle, result: tauri::Result<()>) {
    if let Err(error) = result {
        journal(app, &format!("native-error {error}"));
        let visible = state(app).panel_visible;
        let life = app.state::<Arc<Lifecycle>>();
        let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.panel_error = Some(error.to_string());
        #[cfg(windows)]
        if !visible {
            state.focus.hidden();
        }
        if !visible && !state.destroying && !state.cleaning && state.hidden_since.is_none() {
            state.hidden_since = Some(Instant::now());
        }
        drop(state);
        life.wake();
        let _ = app.emit("panel-error", error.to_string());
        let _ = app.emit("panel-visibility", visible);
    }
}
pub fn take_about_navigation(app: &tauri::AppHandle) -> Result<bool, String> {
    let life = app.state::<Arc<Lifecycle>>();
    let mut state = life.inner.lock().map_err(|_| "弹层状态不可用")?;
    Ok(std::mem::take(&mut state.about_pending))
}
pub fn show_panel(app: &tauri::AppHandle) -> tauri::Result<()> {
    // Cancel even an already queued release before waiting for the GUI turn.
    let life = app.state::<Arc<Lifecycle>>();
    let mut state = life.inner.lock().unwrap_or_else(|e| e.into_inner());
    state.generation = state.generation.wrapping_add(1);
    state.hidden_since = None;
    drop(state);
    life.wake();
    let handle = app.clone();
    app.run_on_main_thread(move || report_native(&handle, show_native(&handle)))
}
pub fn hide_panel(app: &tauri::AppHandle) -> tauri::Result<()> {
    let handle = app.clone();
    app.run_on_main_thread(move || report_native(&handle, hide_native(&handle)))
}
#[cfg(not(windows))]
pub fn toggle_panel(app: &tauri::AppHandle) -> tauri::Result<()> {
    let handle = app.clone();
    app.run_on_main_thread(move || {
        if state(&handle).panel_visible {
            report_native(&handle, hide_native(&handle));
        } else {
            report_native(&handle, show_native(&handle));
        }
    })
}
