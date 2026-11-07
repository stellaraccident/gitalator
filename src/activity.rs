//! Low-volume console summaries; detailed operation records remain in operations.jsonl.
use crate::cache::Span;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
struct Active {
    label: String,
    phase: String,
    since: Instant,
}
struct State {
    active: BTreeMap<u64, Active>,
    last: Option<Instant>,
    completed: u64,
    failed: u64,
}
pub struct Activity {
    pub enabled: AtomicBool,
    next: AtomicU64,
    state: Mutex<State>,
}
impl Default for Activity {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            next: AtomicU64::new(1),
            state: Mutex::new(State {
                active: BTreeMap::new(),
                last: None,
                completed: 0,
                failed: 0,
            }),
        }
    }
}
impl Activity {
    pub fn begin(
        self: &Arc<Self>,
        root: &Path,
        operation: &str,
        label: String,
        detail: &str,
    ) -> Guard {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.state.lock().unwrap().active.insert(
            id,
            Active {
                label,
                phase: "waiting for repository lock".into(),
                since: Instant::now(),
            },
        );
        let span = Arc::new(Span::new(root, operation));
        span.event("requested", Some(detail));
        self.report();
        Guard {
            activity: self.clone(),
            id,
            span,
            finished: false,
        }
    }
    pub fn report(&self) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let mut s = self.state.lock().unwrap();
        if s.last.is_some_and(|t| t.elapsed() < Duration::from_secs(5))
            || (s.active.is_empty() && s.completed == 0 && s.failed == 0)
        {
            return;
        }
        let active = s
            .active
            .values()
            .take(3)
            .map(|a| {
                format!(
                    "{}: {} ({:.1}s)",
                    a.label,
                    a.phase,
                    a.since.elapsed().as_secs_f64()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        eprintln!(
            "gtl: backend: {} active, {} completed, {} failed{}{}",
            s.active.len(),
            s.completed,
            s.failed,
            if active.is_empty() { "" } else { "; " },
            active
        );
        s.completed = 0;
        s.failed = 0;
        s.last = Some(Instant::now());
    }
}
pub struct Guard {
    activity: Arc<Activity>,
    id: u64,
    span: Arc<Span>,
    finished: bool,
}
impl Guard {
    pub fn phase(&self, phase: &str) {
        if let Some(a) = self.activity.state.lock().unwrap().active.get_mut(&self.id) {
            a.phase = phase.into();
        }
        self.span.event("phase", Some(phase));
    }
    pub fn event(&self, status: &str, detail: &str) {
        self.span.event(status, Some(detail));
    }
    pub fn progress(&self) -> crate::backend::Progress {
        let span = self.span.clone();
        Arc::new(move |line| span.event("git-progress", Some(line)))
    }
    pub fn finish<T>(&mut self, result: &crate::Result<T>) {
        match result {
            Ok(_) => self.span.event("succeeded", None),
            Err(e) => {
                let error = crate::backend::redact(&e.to_string());
                self.span.event("failed", Some(&error));
                if self.activity.enabled.load(Ordering::Relaxed) {
                    eprintln!(
                        "gtl: {}: {error} (details: operations.jsonl)",
                        self.span.operation()
                    );
                }
            }
        }
        let mut s = self.activity.state.lock().unwrap();
        s.active.remove(&self.id);
        if result.is_ok() {
            s.completed += 1;
        } else {
            s.failed += 1;
        }
        self.finished = true;
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if !self.finished {
            self.span.event("failed", Some("operation abandoned"));
            let mut s = self.activity.state.lock().unwrap();
            s.active.remove(&self.id);
            s.failed += 1;
        }
    }
}
