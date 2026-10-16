// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Serial history writer. Reservations include the batch currently in SQLite.
use pureflow_storage::{HistoryEvent, HistoryStore};
use std::{
    sync::{
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_EVENTS: usize = 120;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const COMMIT_AFTER: Duration = Duration::from_secs(10);
type Reply = SyncSender<Result<(), String>>;
enum Message {
    Event(HistoryEvent, Instant),
    Flush,
    Barrier(Reply),
    Retry(Reply),
    Compact(Reply),
    Stop(Reply),
}
#[derive(Default)]
struct Budget {
    events: usize,
    bytes: usize,
    last_accepted: Option<u64>,
    error: Option<String>,
    closed: bool,
}
#[derive(Clone)]
pub struct HistoryWriter {
    sender: SyncSender<Message>,
    budget: Arc<Mutex<Budget>>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
}
impl HistoryWriter {
    pub fn start(
        store: HistoryStore,
        report: impl Fn(String) + Send + 'static,
    ) -> Result<Self, String> {
        // Eight additional control slots; event reservations remain bounded at 120.
        let (sender, receiver) = mpsc::sync_channel(MAX_EVENTS + 8);
        let budget = Arc::new(Mutex::new(Budget {
            last_accepted: store.persisted_time()?,
            ..Budget::default()
        }));
        let worker_budget = budget.clone();
        let worker = thread::Builder::new()
            .name("pureflow-history".into())
            .spawn(move || run(store, receiver, worker_budget, report))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            sender,
            budget,
            worker: Arc::new(Mutex::new(Some(worker))),
        })
    }
    pub fn error(&self) -> Option<String> {
        self.budget
            .lock()
            .map(|b| b.error.clone())
            .unwrap_or_else(|_| Some("历史写入状态不可用".into()))
    }
    pub fn enqueue(&self, event: HistoryEvent) -> Result<(), String> {
        let bytes = event.retained_bytes();
        let mut budget = self.budget.lock().map_err(|_| "历史写入状态不可用")?;
        if budget.closed {
            return Err("历史写入线程正在退出".into());
        }
        if let Some(error) = &budget.error {
            return Err(error.clone());
        }
        if budget.events >= MAX_EVENTS || bytes > MAX_BYTES.saturating_sub(budget.bytes) {
            let error = "历史待写队列达到 120 个事件或 32 MiB 上限；已停止采集，请显式重试（未保存数据仍保留）".to_string();
            budget.error = Some(error.clone());
            return Err(error);
        }
        event.validate_after(budget.last_accepted)?;
        let end = event.unix_ms();
        budget.events += 1;
        budget.bytes += bytes;
        if self
            .sender
            .try_send(Message::Event(event, Instant::now()))
            .is_err()
        {
            budget.events -= 1;
            budget.bytes -= bytes;
            let error = "历史写入队列不可用；已停止采集，请显式重试".to_string();
            budget.error = Some(error.clone());
            return Err(error);
        }
        budget.last_accepted = Some(end);
        if budget.events == MAX_EVENTS || budget.bytes == MAX_BYTES {
            budget.error = Some(
                "历史待写队列达到 120 个事件或 32 MiB 上限；已停止采集，待写数据仍保留，请显式重试"
                    .into(),
            );
        }
        Ok(())
    }
    /// Schedule an early FIFO commit without blocking sampling/control on SQLite.
    /// Pause only promises to stop and schedule the tail; failures use the report callback.
    /// Query/export, compaction, retry and shutdown use acknowledged commands instead.
    pub fn request_flush(&self) -> Result<(), String> {
        let budget = self.budget.lock().map_err(|_| "历史写入状态不可用")?;
        if budget.closed {
            return Err("历史写入线程正在退出".into());
        }
        if let Some(error) = &budget.error {
            return Err(error.clone());
        }
        self.sender
            .try_send(Message::Flush)
            .map_err(|_| "历史落盘请求繁忙".into())
    }
    fn request(&self, make: impl FnOnce(Reply) -> Message) -> Result<(), String> {
        let (reply, receive) = mpsc::sync_channel(1);
        {
            let budget = self.budget.lock().map_err(|_| "历史写入状态不可用")?;
            if budget.closed {
                return Err("历史写入线程正在退出".into());
            }
            self.sender
                .try_send(make(reply))
                .map_err(|_| "历史写入屏障繁忙或不可用")?;
        }
        receive.recv().map_err(|_| "历史写入线程已停止")?
    }
    /// FIFO boundary: subsequent samples can queue while this transaction is in progress.
    pub fn barrier(&self) -> Result<(), String> {
        if let Some(error) = self.error() {
            return Err(error);
        }
        self.request(Message::Barrier)
    }
    /// No automatic retry: only the explicit user action may resubmit a retained failed batch.
    pub fn retry(&self) -> Result<(), String> {
        self.request(Message::Retry)
    }
    pub fn compact(&self) -> Result<(), String> {
        self.request(Message::Compact)
    }
    pub fn shutdown(&self) -> Result<(), String> {
        let mut worker = self.worker.lock().map_err(|_| "历史写入线程不可用")?;
        let Some(handle) = worker.take() else {
            return Ok(());
        };
        // Seal event admission before Stop; enqueue holds this lock through try_send.
        self.budget.lock().map_err(|_| "历史写入状态不可用")?.closed = true;
        let (reply, receive) = mpsc::sync_channel(1);
        self.sender
            .send(Message::Stop(reply))
            .map_err(|_| "历史写入线程已停止")?;
        let result = receive.recv().map_err(|_| "历史写入线程已停止")?;
        handle.join().map_err(|_| "历史写入线程异常退出")?;
        result
    }
}
fn flush(
    store: &mut HistoryStore,
    pending: &mut Vec<HistoryEvent>,
    budget: &Mutex<Budget>,
) -> Result<(), String> {
    store.record_batch(pending)?;
    let bytes = pending
        .iter()
        .map(HistoryEvent::retained_bytes)
        .sum::<usize>();
    let mut state = budget.lock().unwrap_or_else(|e| e.into_inner());
    state.events -= pending.len();
    state.bytes -= bytes;
    pending.clear();
    drop(state);
    // This API itself keeps the original hourly maintenance cadence.
    if let Err(error) = store.maintenance() {
        eprintln!("历史空间维护失败：{error}");
    }
    Ok(())
}
fn run(
    mut store: HistoryStore,
    receiver: Receiver<Message>,
    budget: Arc<Mutex<Budget>>,
    report: impl Fn(String),
) {
    let mut pending = Vec::new();
    let mut oldest: Option<Instant> = None;
    loop {
        let failed = budget
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone();
        let message = if failed.is_none() && oldest.is_some_and(|at| at.elapsed() >= COMMIT_AFTER) {
            Message::Flush
        } else if let Some(at) = oldest.filter(|_| failed.is_none()) {
            match receiver.recv_timeout(COMMIT_AFTER.saturating_sub(at.elapsed())) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => Message::Flush,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match receiver.recv() {
                Ok(message) => message,
                Err(_) => break,
            }
        };
        let (reply, retry, stop, compact) = match message {
            Message::Event(event, at) => {
                oldest.get_or_insert(at);
                pending.push(event);
                continue;
            }
            Message::Flush => (None, false, false, false),
            Message::Barrier(reply) => (Some(reply), false, false, false),
            Message::Retry(reply) => (Some(reply), true, false, false),
            Message::Stop(reply) => (Some(reply), false, true, false),
            Message::Compact(reply) => (Some(reply), false, false, true),
        };
        let failed = budget
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone();
        let had_error = failed.is_some();
        let result = if let Some(error) = failed.filter(|_| !retry) {
            Err(error)
        } else {
            flush(&mut store, &mut pending, &budget).and_then(|()| {
                if compact {
                    store.compact()
                } else {
                    Ok(())
                }
            })
        };
        if let Err(error) = &result {
            if !had_error || retry {
                let message = format!(
                    "历史存储失败：{error}；采集已停止，未保存数据仍保留，请修复存储后显式重试"
                );
                budget.lock().unwrap_or_else(|e| e.into_inner()).error = Some(message.clone());
                report(message);
            }
        } else {
            oldest = None;
            if retry {
                budget.lock().unwrap_or_else(|e| e.into_inner()).error = None;
            }
        }
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
        if stop {
            break;
        }
    }
}
