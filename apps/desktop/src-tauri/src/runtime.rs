// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
use crate::model::{SessionModel, Snapshot};
use pureflow_collector::clock::SampleClock;
#[cfg(not(windows))]
use pureflow_collector::NativeCollector;
use pureflow_core::CounterEngine;
use pureflow_storage::{HistoryQuery, HistoryResult, HistoryStore};
use serde::{ser::SerializeStruct, Serialize, Serializer};
#[cfg(windows)]
use std::time::Duration;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};
#[cfg(windows)]
type RuntimeCollector = crate::helper::HelperCollector;
#[cfg(not(windows))]
type RuntimeCollector = NativeCollector;
type HistoryBackend = crate::history_writer::HistoryWriter;

fn history_gap(history: &HistoryBackend, now: u64) -> Result<(), String> {
    history.enqueue(pureflow_storage::HistoryEvent::Gap(now))?;
    history.error().map_or(Ok(()), Err)
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}
struct Shared {
    model: SessionModel,
    generation: u64,
    retry_generation: u64,
    revision: u64,
    cached: Option<Arc<Snapshot>>,
    #[cfg(windows)]
    cleanup_error: Option<String>,
}
impl Shared {
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.cached = None;
    }
}
pub struct SnapshotResponse {
    revision: String,
    snapshot: Option<Arc<Snapshot>>,
}
impl Serialize for SnapshotResponse {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SnapshotResponse", 2)?;
        state.serialize_field("revision", &self.revision)?;
        state.serialize_field("snapshot", &self.snapshot.as_deref())?;
        state.end()
    }
}
pub struct TrayStatus {
    pub phase: String,
    pub status: String,
    pub paused: bool,
    pub download_bps: f64,
    pub upload_bps: f64,
}
#[derive(Default)]
struct Notifications(Mutex<Vec<SyncSender<()>>>);
impl Notifications {
    fn send(&self) {
        if let Ok(mut senders) = self.0.lock() {
            senders.retain(|s| !matches!(s.try_send(()), Err(mpsc::TrySendError::Disconnected(_))));
        }
    }
}
pub struct Monitor {
    shared: Arc<Mutex<Shared>>,
    transition: Arc<Mutex<()>>,
    stop: Arc<AtomicBool>,
    wake: SyncSender<()>,
    notifications: Arc<Notifications>,
    worker: Mutex<Option<JoinHandle<()>>>,
    finished: Mutex<Receiver<()>>,
    history: HistoryBackend,
    reader: Mutex<HistoryStore>,
    read_cancel: Mutex<Option<Arc<AtomicBool>>>,
    database: PathBuf,
}
impl Monitor {
    pub fn start(database: &Path) -> Result<Self, String> {
        let store = HistoryStore::open(database)?;
        let reader = Mutex::new(HistoryStore::open_readonly(database)?);
        let shared = Arc::new(Mutex::new(Shared {
            model: SessionModel::new(unix_ms()),
            generation: 0,
            retry_generation: 0,
            revision: 0,
            cached: None,
            #[cfg(windows)]
            cleanup_error: None,
        }));
        let transition = Arc::new(Mutex::new(()));
        let worker_transition = Arc::clone(&transition);
        let stop = Arc::new(AtomicBool::new(false));
        let notifications = Arc::new(Notifications::default());
        let (wake, receiver) = mpsc::sync_channel(1);
        let (done, finished) = mpsc::sync_channel(1);
        let history = {
            let shared = shared.clone();
            let notifications = notifications.clone();
            let wake = wake.clone();
            crate::history_writer::HistoryWriter::start(store, move |error| {
                if let Ok(mut state) = shared.lock() {
                    set_error(&mut state, error);
                }
                notifications.send();
                let _ = wake.try_send(());
            })?
        };
        let worker_shared = Arc::clone(&shared);
        let worker_history = history.clone();
        let worker_stop = Arc::clone(&stop);
        let worker_notifications = Arc::clone(&notifications);
        let worker = thread::Builder::new()
            .name("pureflow-sampler".into())
            .spawn(move || {
                run(
                    worker_shared,
                    receiver,
                    worker_history,
                    worker_stop,
                    worker_notifications,
                    worker_transition,
                );
                let _ = done.send(());
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            shared,
            transition,
            stop,
            wake,
            notifications,
            worker: Mutex::new(Some(worker)),
            finished: Mutex::new(finished),
            history,
            reader,
            read_cancel: Mutex::new(None),
            database: database.to_path_buf(),
        })
    }
    #[cfg(windows)]
    pub fn database_path(&self) -> &Path {
        &self.database
    }
    pub fn subscribe(&self) -> (SyncSender<()>, Receiver<()>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        if let Ok(mut senders) = self.notifications.0.lock() {
            senders.push(sender.clone());
        }
        (sender, receiver)
    }
    pub fn cancel_history(&self) {
        if let Ok(mut cancel) = self.read_cancel.lock() {
            if let Some(token) = cancel.take() {
                token.store(true, Ordering::Release);
            }
        }
    }
    pub fn history(&self, query: HistoryQuery) -> Result<HistoryResult, String> {
        if self.stop.load(Ordering::Acquire) {
            return Err("应用正在退出".into());
        }
        let token = Arc::new(AtomicBool::new(false));
        {
            let mut current = self.read_cancel.lock().map_err(|_| "历史取消状态不可用")?;
            if let Some(old) = current.replace(Arc::clone(&token)) {
                old.store(true, Ordering::Release);
            }
        }
        // Frontend permits one in-flight + one latest request. Backend also refuses unbounded queues.
        let reader = self
            .reader
            .try_lock()
            .map_err(|_| "历史查询繁忙，请稍后重试")?;
        self.history.barrier()?;
        reader.query_with_cancel(query, &token)
    }
    pub fn export_csv(&self, query: HistoryQuery) -> Result<String, String> {
        if self.stop.load(Ordering::Acquire) {
            return Err("应用正在退出".into());
        }
        let reader = self
            .reader
            .try_lock()
            .map_err(|_| "历史查询繁忙，请稍后重试")?;
        self.history.barrier()?;
        reader.export_csv(query)
    }
    pub fn compact_history(&self) -> Result<String, String> {
        self.cancel_history();
        // Transition reservation prevents resume while compaction releases the short-lived state lock.
        let _transition = self.transition.lock().map_err(|_| "采集控制不可用")?;
        let shared = self.shared.lock().map_err(|_| "统计状态不可用")?;
        if !shared.model.snapshot.paused {
            return Err("请先暂停采集，再整理历史文件".into());
        }
        if self.stop.load(Ordering::Acquire) {
            return Err("应用正在退出".into());
        }
        drop(shared);
        let _reader = self
            .reader
            .try_lock()
            .map_err(|_| "历史查询尚未结束，请稍后重试")?;
        crate::storage_paths::ensure_compaction_space(&self.database)?;
        self.history.compact()?;
        Ok("历史文件已整理，统计记录未删除".into())
    }
    pub fn revision(&self) -> Result<String, String> {
        Ok(self
            .shared
            .lock()
            .map_err(|_| "统计状态不可用")?
            .revision
            .to_string())
    }
    pub fn tray_status(&self) -> Result<TrayStatus, String> {
        let shared = self.shared.lock().map_err(|_| "统计状态不可用")?;
        let snapshot = &shared.model.snapshot;
        Ok(TrayStatus {
            phase: snapshot.phase.clone(),
            status: snapshot.status.clone(),
            paused: snapshot.paused,
            download_bps: snapshot.download_bps,
            upload_bps: snapshot.upload_bps,
        })
    }
    pub fn snapshot(&self, known: Option<String>) -> Result<SnapshotResponse, String> {
        let mut shared = self.shared.lock().map_err(|_| "统计状态不可用")?;
        let revision = shared.revision.to_string();
        let snapshot = if known.as_deref() == Some(&revision) {
            None
        } else {
            if shared.cached.is_none() {
                shared.cached = Some(Arc::new(shared.model.materialize()));
            }
            shared.cached.clone()
        };
        Ok(SnapshotResponse { revision, snapshot })
    }
    pub fn pause(&self, paused: bool) -> Result<(), String> {
        let _transition = self.transition.try_lock().map_err(|_| "采集控制不可用")?;
        if self.stop.load(Ordering::Acquire) {
            return Err("应用正在退出".into());
        }
        let mut shared = self.shared.lock().map_err(|_| "统计状态不可用")?;
        #[cfg(any(windows, target_os = "macos"))]
        if shared.model.snapshot.phase == "error" {
            #[cfg(windows)]
            return Err("采集异常后仅允许显式重新采集；暂停/继续不会重新请求 UAC".into());
            #[cfg(not(windows))]
            return Err("采集异常后仅允许显式重新采集；请先修复错误再重试".into());
        }
        if shared.model.snapshot.paused == paused {
            return Ok(());
        }
        shared.generation = shared.generation.wrapping_add(1);
        shared.model.snapshot.paused = paused;
        shared.model.snapshot.phase = if paused { "paused" } else { "starting" }.into();
        shared.model.snapshot.status = if paused {
            "已暂停 · 不记录暂停期间流量"
        } else {
            "正在恢复 · 重新建立基线"
        }
        .into();
        let now = unix_ms();
        shared.model.gap(now);
        shared.changed();
        drop(shared);
        self.notifications.send();
        let _ = self.wake.try_send(());
        if paused {
            if let Err(error) =
                history_gap(&self.history, now).and_then(|()| self.history.request_flush())
            {
                if let Ok(mut state) = self.shared.lock() {
                    set_error(&mut state, error.clone());
                }
                self.notifications.send();
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn retry(&self) -> Result<(), String> {
        let _transition = self.transition.try_lock().map_err(|_| "采集控制不可用")?;
        if self.stop.load(Ordering::Acquire) {
            return Err("应用正在退出".into());
        }
        self.history.retry()?;
        let mut shared = self.shared.lock().map_err(|_| "统计状态不可用")?;
        shared.generation = shared.generation.wrapping_add(1);
        shared.retry_generation = shared.retry_generation.wrapping_add(1);
        shared.model.snapshot.paused = false;
        shared.model.snapshot.phase = "starting".into();
        shared.model.snapshot.status = "正在重新启动采集…".into();
        shared.model.gap(unix_ms());
        shared.changed();
        drop(shared);
        self.notifications.send();
        let _ = self.wake.try_send(());
        Ok(())
    }
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        self.cancel_history();
        let _ = self.wake.try_send(());
        self.notifications.send();
        if let Ok(mut worker) = self.worker.lock() {
            if let Some(handle) = worker.take() {
                // ShellExecute UAC is OS-controlled; never wait indefinitely for consent.
                #[cfg(windows)]
                let finished = self
                    .finished
                    .lock()
                    .ok()
                    .is_some_and(|r| r.recv_timeout(Duration::from_secs(6)).is_ok());
                #[cfg(not(windows))]
                let finished = self.finished.lock().is_ok_and(|r| r.recv().is_ok());
                if finished {
                    let _ = handle.join();
                } else {
                    // Detach the sampler, but still commit history accepted before stop.
                    eprintln!(
                        "Sampler shutdown deadline reached; parent-death cleanup remains active"
                    );
                }
                #[cfg(windows)]
                if let Ok(state) = self.shared.lock() {
                    if let Some(error) = &state.cleanup_error {
                        eprintln!("PureFlow shutdown cleanup not confirmed: {error}");
                    }
                }
            }
        }
        // Serialize the final history boundary with pause/retry/commit, even if UAC is still open.
        let _transition = self.transition.lock();
        let _reader = self.reader.lock(); // cancelled queries/active exports finish before the tail write
        if let Err(error) = self.history.shutdown() {
            eprintln!("正常退出未能保存全部历史：{error}");
            rfd::MessageDialog::new()
                .set_title("PureFlow 退出保存失败")
                .set_description(format!(
                    "尚未保存的历史无法落盘，退出将丢失这些记录。\n{error}"
                ))
                .set_level(rfd::MessageLevel::Error)
                .show();
            if let Ok(mut state) = self.shared.lock() {
                set_error(&mut state, format!("退出保存失败：{error}"));
            }
        }
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn start_collector(stop: &Arc<AtomicBool>) -> Result<RuntimeCollector, String> {
    #[cfg(windows)]
    {
        RuntimeCollector::start_with_cancel(Arc::clone(stop))
    }
    #[cfg(not(windows))]
    {
        let _ = stop;
        RuntimeCollector::start().map_err(|e| e.to_string())
    }
}
fn set_error(shared: &mut Shared, error: String) {
    shared.model.snapshot.phase = "error".into();
    shared.model.snapshot.status = error;
    shared.model.gap(unix_ms());
    shared.changed();
}
#[cfg(windows)]
fn close_collector(
    collector: &mut Option<RuntimeCollector>,
    shared: &Mutex<Shared>,
    notifications: &Notifications,
    expected_generation: u64,
) -> bool {
    let Some(mut native) = collector.take() else {
        return true;
    };
    if let Err(error) = native.shutdown() {
        let message = format!("采集器退出未确认：{error}；请确认旧采集器已退出后显式重试");
        if let Ok(mut state) = shared.lock() {
            state.cleanup_error = Some(message.clone());
            let previous = state.model.snapshot.status.clone();
            if expected_generation == state.generation {
                set_error(&mut state, format!("{previous}；{message}"));
            }
        }
        notifications.send();
        return false;
    }
    true
}
fn run(
    shared: Arc<Mutex<Shared>>,
    receiver: Receiver<()>,
    history: HistoryBackend,
    stop: Arc<AtomicBool>,
    notifications: Arc<Notifications>,
    transition: Arc<Mutex<()>>,
) {
    let mut collector: Option<RuntimeCollector> = None;
    let mut engine = CounterEngine::default();
    let mut clock = None;
    let mut generation = u64::MAX;
    let mut retry_generation = 0;
    let mut wall_anchor: Option<(u64, u64)> = None;
    let mut deadline = std::time::Instant::now();
    #[cfg(windows)]
    let mut was_paused = false;
    while !stop.load(Ordering::Acquire) {
        let control_guard = match transition.lock() {
            Ok(guard) => guard,
            Err(_) => break,
        };
        // Shutdown seals history under the same reservation, including late UAC completion.
        if stop.load(Ordering::Acquire) {
            break;
        }
        let (next_generation, next_retry, paused, failed) = match shared.lock() {
            Ok(state) => (
                state.generation,
                state.retry_generation,
                state.model.snapshot.paused,
                state.model.snapshot.phase == "error",
            ),
            Err(_) => break,
        };
        if next_generation != generation {
            if next_retry != retry_generation {
                #[cfg(windows)]
                if !close_collector(&mut collector, &shared, &notifications, next_generation) {
                    retry_generation = next_retry;
                    generation = next_generation;
                    continue;
                }
                #[cfg(not(windows))]
                {
                    collector = None;
                }
                #[cfg(windows)]
                {
                    was_paused = false;
                }
            }
            retry_generation = next_retry;
            engine.reset();
            clock = None;
            wall_anchor = None;
            generation = next_generation;
            deadline = std::time::Instant::now();
            let gap = if paused {
                Ok(())
            } else {
                history_gap(&history, unix_ms())
            };
            let gap = gap.and_then(|()| {
                if paused {
                    history.request_flush()
                } else {
                    Ok(())
                }
            });
            if let Err(error) = gap {
                if let Ok(mut state) = shared.lock() {
                    set_error(&mut state, error);
                }
                notifications.send();
                continue;
            }
        }
        drop(control_guard);
        #[cfg(windows)]
        if paused != was_paused {
            if let Some(native) = collector.as_mut() {
                if let Err(error) = native.pause(paused) {
                    if let Ok(mut state) = shared.lock() {
                        if state.generation == generation {
                            set_error(&mut state, error);
                        }
                    }
                    notifications.send();
                    close_collector(&mut collector, &shared, &notifications, generation);
                    was_paused = paused;
                    continue;
                }
            }
        }
        #[cfg(not(windows))]
        if paused {
            collector = None;
        }
        #[cfg(windows)]
        {
            was_paused = paused;
        }
        if failed {
            #[cfg(windows)]
            close_collector(&mut collector, &shared, &notifications, generation);
            #[cfg(not(windows))]
            {
                collector = None;
            }
        }
        if paused || failed {
            if receiver.recv().is_err() {
                break;
            }
            continue;
        }
        if collector.is_none() {
            match start_collector(&stop).and_then(|native| {
                SampleClock::start()
                    .map(|time| (native, time))
                    .map_err(|e| e.to_string())
            }) {
                Ok((native, time)) => {
                    #[cfg(windows)]
                    if let Ok(mut state) = shared.lock() {
                        state.cleanup_error = None;
                    }
                    collector = Some(native);
                    clock = Some(time);
                    deadline = std::time::Instant::now();
                }
                Err(error) => {
                    if let Ok(mut state) = shared.lock() {
                        if state.generation == generation && !stop.load(Ordering::Acquire) {
                            set_error(&mut state, error);
                        }
                    }
                    notifications.send();
                    continue;
                }
            }
        }
        if clock.is_none() {
            match SampleClock::start() {
                Ok(time) => {
                    clock = Some(time);
                    deadline = std::time::Instant::now();
                }
                Err(error) => {
                    if let Ok(mut state) = shared.lock() {
                        set_error(&mut state, error.to_string());
                    }
                    notifications.send();
                    continue;
                }
            }
        }
        if deadline > std::time::Instant::now() {
            match receiver
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                Ok(()) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        let sampled = collector
            .as_mut()
            .expect("collector initialized")
            .sample()
            .map_err(|e| e.to_string())
            .and_then(|sample| {
                clock
                    .as_mut()
                    .expect("clock initialized")
                    .sample()
                    .map(|(ms, suspended)| (sample, ms, suspended))
                    .map_err(|e| e.to_string())
            });
        let Ok(commit_guard) = transition.lock() else {
            break;
        };
        let Ok(mut state) = shared.lock() else {
            break;
        };
        if state.generation != generation
            || stop.load(Ordering::Acquire)
            || state.model.snapshot.paused
            || state.model.snapshot.phase == "error"
        {
            drop(state);
            continue;
        }
        match sampled {
            Err(error) => {
                set_error(&mut state, error);
                engine.reset();
            }
            Ok((sample, monotonic_ms, suspended)) => {
                let observed = unix_ms();
                let now = wall_anchor
                    .map(|(wall, mono)| wall.saturating_add(monotonic_ms.saturating_sub(mono)))
                    .unwrap_or(observed);
                if observed.abs_diff(now) > 2000 {
                    engine.reset();
                    set_error(
                        &mut state,
                        "系统时间发生跳变，已停止记录以避免历史重叠；校准时间后重新采集".into(),
                    );
                } else {
                    wall_anchor.get_or_insert((now, monotonic_ms));
                    let warnings = sample.warnings.clone();
                    if suspended {
                        engine.reset();
                        state.model.gap(now);
                        drop(state);
                        let gap_result = history_gap(&history, now);
                        let gap_result = gap_result.and_then(|()| history.request_flush());
                        state = match shared.lock() {
                            Ok(state) => state,
                            Err(_) => break,
                        };
                        if let Err(error) = gap_result {
                            set_error(&mut state, error);
                        }
                    }
                    if state.model.snapshot.phase != "error" {
                        match engine.apply(monotonic_ms, sample) {
                            Ok(Some(record)) => {
                                let prepared = state.model.prepare(&record);
                                let prepared = prepared.and_then(|prepared| {
                                    history.enqueue(pureflow_storage::HistoryEvent::Record {
                                        unix_ms: now,
                                        record: record.clone(),
                                    })?;
                                    Ok(prepared)
                                });
                                match prepared {
                                    Ok(prepared) => {
                                        state.model.commit(now, record, prepared);
                                        state.changed();
                                    }
                                    Err(error) => {
                                        set_error(&mut state, format!("记录失败：{error}"));
                                        engine.reset();
                                    }
                                }
                            }
                            Ok(None) => {
                                if let Err(error) = if suspended {
                                    Ok(())
                                } else {
                                    history_gap(&history, now)
                                } {
                                    set_error(&mut state, format!("记录缺测失败：{error}"));
                                    engine.reset();
                                    drop(state);
                                    drop(commit_guard);
                                    notifications.send();
                                    continue;
                                }
                                state.model.snapshot.last_sample_ms = Some(now);
                                state.model.snapshot.phase = "starting".into();
                                state.model.snapshot.status =
                                    crate::sampling_policy::BASELINE.into();
                                state.model.snapshot.warnings = warnings;
                                state.model.gap(now);
                                state.changed();
                            }
                            Err(error) => {
                                set_error(&mut state, error.to_string());
                                engine.reset();
                            }
                        }
                    }
                }
            }
        }
        if let Some(error) = history.error() {
            set_error(&mut state, error);
            engine.reset();
        }
        if state.model.snapshot.phase == "error" && history.error().is_none() {
            // Sampling failures also terminate coverage and schedule the accepted tail.
            // A backward wall-clock jump may reject this gap rather than corrupt history.
            if let Err(error) =
                history_gap(&history, unix_ms()).and_then(|()| history.request_flush())
            {
                let original = state.model.snapshot.status.clone();
                set_error(&mut state, format!("{original}；缺测记录失败：{error}"));
            }
        }
        let failed = state.model.snapshot.phase == "error";
        drop(state);
        drop(commit_guard);
        notifications.send();
        if failed {
            #[cfg(windows)]
            close_collector(&mut collector, &shared, &notifications, generation);
            #[cfg(not(windows))]
            {
                collector = None;
            }
        }
        deadline = crate::sampling_policy::next_tick(deadline, std::time::Instant::now());
    }
    #[cfg(windows)]
    close_collector(&mut collector, &shared, &notifications, generation);
    drop(collector);
}
