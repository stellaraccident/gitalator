//! Observable, cooperatively cancellable operations. No workspace locks are
//! needed to inspect or cancel activity.
use crate::{error, Result};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU64 = AtomicU64::new(1);
static ACTIVE: OnceLock<Mutex<BTreeMap<u64, Arc<State>>>> = OnceLock::new();
static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();
thread_local! {
    static CURRENT: RefCell<Option<Arc<State>>> = const { RefCell::new(None) };
    static STOP: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
    static CRITICAL: Cell<usize> = const { Cell::new(0) };
}
struct State {
    id: u64,
    label: String,
    start: Instant,
    cancelled: AtomicBool,
    stage: Mutex<String>,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
fn active() -> &'static Mutex<BTreeMap<u64, Arc<State>>> {
    ACTIVE.get_or_init(Default::default)
}
pub struct Task {
    state: Arc<State>,
}
impl Task {
    pub fn new(label: impl Into<String>) -> Self {
        let state = Arc::new(State {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            label: label.into(),
            start: Instant::now(),
            cancelled: AtomicBool::new(false),
            stage: Mutex::new("queued".into()),
            wake: Mutex::new(None),
        });
        active().lock().unwrap().insert(state.id, state.clone());
        Self { state }
    }
    pub fn status(&self) -> Value {
        json!({"id":self.state.id,"operation":self.state.label,"elapsed_seconds":self.state.start.elapsed().as_secs_f64(),"stage":*self.state.stage.lock().unwrap()})
    }
    pub fn id(&self) -> u64 {
        self.state.id
    }
    pub fn finish(&self) {
        active().lock().unwrap().remove(&self.state.id);
    }
    pub fn on_cancel(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.state.wake.lock().unwrap() = Some(Arc::new(callback));
        if self.state.cancelled.load(Ordering::Acquire) {
            cancel_state(&self.state);
        }
    }
    pub fn cancel(&self) {
        cancel_state(&self.state);
    }
    pub fn enter(&self) -> Context {
        let old = CURRENT.with(|c| c.replace(Some(self.state.clone())));
        *self.state.stage.lock().unwrap() = "running".into();
        Context(old, STOP.with(|c| c.borrow().clone()))
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        active().lock().unwrap().remove(&self.state.id);
    }
}
pub struct Context(Option<Arc<State>>, Option<Arc<AtomicBool>>);
impl Drop for Context {
    fn drop(&mut self) {
        CURRENT.with(|c| c.replace(self.0.take()));
        STOP.with(|c| c.replace(self.1.take()));
    }
}
pub fn check() -> Result<()> {
    if CRITICAL.with(Cell::get) > 0 {
        return Ok(());
    }
    if STOP.with(|c| {
        c.borrow()
            .as_ref()
            .is_some_and(|s| s.load(Ordering::Acquire))
    }) || SHUTDOWN.load(Ordering::Acquire)
        || CURRENT.with(|c| {
            c.borrow()
                .as_ref()
                .is_some_and(|s| s.cancelled.load(Ordering::Acquire))
        })
    {
        return Err(std::io::Error::from_raw_os_error(libc::EINTR).into());
    }
    Ok(())
}
pub fn interrupted(e: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.raw_os_error() == Some(libc::EINTR))
}
pub struct Critical;
pub fn critical() -> Critical {
    CRITICAL.with(|v| v.set(v.get() + 1));
    Critical
}
impl Drop for Critical {
    fn drop(&mut self) {
        CRITICAL.with(|v| v.set(v.get() - 1));
    }
}
pub fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    loop {
        check()?;
        match mutex.try_lock() {
            Ok(g) => return Ok(g),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => return Err(error("state lock poisoned")),
        }
    }
}
pub fn snapshot() -> Value {
    let states: Vec<_> = active().lock().unwrap().values().cloned().collect();
    json!(states
        .iter()
        .map(|s| json!({"id":s.id,"operation":s.label,
        "elapsed_seconds":s.start.elapsed().as_secs_f64(),
        "stage":*s.stage.lock().unwrap(),"cancelled":s.cancelled.load(Ordering::Acquire)}))
        .collect::<Vec<_>>())
}
pub fn cancel(id: u64) -> bool {
    let state = active().lock().unwrap().get(&id).cloned();
    if let Some(s) = state {
        cancel_state(&s);
        event(
            "info",
            &format!("Cancellation requested for operation {id}: {}", s.label),
        );
        true
    } else {
        false
    }
}
fn cancel_state(state: &State) {
    state.cancelled.store(true, Ordering::Release);
    let wake = state.wake.lock().unwrap().clone();
    if let Some(wake) = wake {
        wake();
    }
}
pub fn cancel_all() {
    let states: Vec<_> = active().lock().unwrap().values().cloned().collect();
    for state in states {
        cancel_state(&state);
    }
}
pub fn stage(message: &str) {
    CURRENT.with(|c| {
        if let Some(s) = c.borrow().as_ref() {
            *s.stage.lock().unwrap() = message.into();
        }
    });
}
struct Logger {
    path: PathBuf,
    file: File,
    bytes: u64,
    console: bool,
}
pub fn init(root: &Path) -> Result<()> {
    if LOGGER.get().is_some() {
        return Ok(());
    }
    let dir = root.join("logs");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("activity.jsonl");
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let bytes = file.metadata()?.len();
    let _ = LOGGER.set(Mutex::new(Logger {
        path,
        file,
        bytes,
        console: std::io::stderr().is_terminal(),
    }));
    Ok(())
}
pub fn event(level: &str, message: &str) {
    let message = crate::git::redact(message);
    let message: String = message.chars().take(4096).collect();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (id, label) = CURRENT.with(|c| {
        c.borrow()
            .as_ref()
            .map(|s| (Some(s.id), Some(s.label.clone())))
            .unwrap_or_default()
    });
    let line=json!({"time_unix_ms":now.as_millis(),"level":level,"operation_id":id,"operation":label,"message":message}).to_string()+"\n";
    if let Some(logger) = LOGGER.get() {
        let mut l = logger.lock().unwrap();
        if l.bytes >= 10 * 1024 * 1024 {
            let rotate = (|| -> std::io::Result<()> {
                for i in (1..3).rev() {
                    let old = l.path.with_extension(format!("jsonl.{i}"));
                    if old.exists() {
                        std::fs::rename(old, l.path.with_extension(format!("jsonl.{}", i + 1)))?;
                    }
                }
                std::fs::rename(&l.path, l.path.with_extension("jsonl.1"))?;
                l.file = OpenOptions::new().create(true).append(true).open(&l.path)?;
                l.bytes = 0;
                Ok(())
            })();
            if let Err(e) = rotate {
                eprintln!("gtl: activity log rotation failed: {e}");
            }
        }
        if let Err(e) = l.file.write_all(line.as_bytes()) {
            eprintln!("gtl: activity log write failed: {e}");
        }
        l.bytes += line.len() as u64;
        if l.console {
            let t = now.as_secs() % 86400;
            eprintln!(
                "{:02}:{:02}:{:02} {level} [{}] {message}",
                t / 3600,
                t / 60 % 60,
                t % 60,
                id.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
            );
        }
    }
}
pub fn heartbeat() {
    let states: Vec<_> = active().lock().unwrap().values().cloned().collect();
    for s in states {
        if s.start.elapsed() >= Duration::from_secs(1) {
            event(
                "info",
                &format!(
                    "Operation {}: {}; {}; elapsed {:.1}s{}",
                    s.id,
                    s.label,
                    *s.stage.lock().unwrap(),
                    s.start.elapsed().as_secs_f64(),
                    if s.cancelled.load(Ordering::Acquire) {
                        "; cancelling"
                    } else {
                        ""
                    }
                ),
            );
        }
    }
}

#[derive(Clone)]
pub struct Scope(Option<Arc<State>>, Option<Arc<AtomicBool>>);
pub fn capture() -> Scope {
    Scope(
        CURRENT.with(|c| c.borrow().clone()),
        STOP.with(|c| c.borrow().clone()),
    )
}
impl Scope {
    pub(crate) fn with_stop(mut self, stop: Arc<AtomicBool>) -> Self {
        self.1 = Some(stop);
        self
    }
    pub fn enter(&self) -> Context {
        Context(
            CURRENT.with(|c| c.replace(self.0.clone())),
            STOP.with(|c| c.replace(self.1.clone())),
        )
    }
}
