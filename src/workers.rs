use crate::{metadata::Runtime, Result};
use std::sync::{mpsc, Arc, Mutex};
type Job = Box<dyn FnOnce() + Send>;
/// Blocking metadata work never executes on the FUSE dispatch thread.
pub struct Workers {
    sender: mpsc::SyncSender<Job>,
    runtime: Arc<Runtime>,
    pending: Arc<(Mutex<usize>, std::sync::Condvar)>,
}
impl Workers {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Job>(128);
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..4 {
            let receiver = receiver.clone();
            std::thread::spawn(move || loop {
                let job = receiver.lock().unwrap().recv();
                match job {
                    Ok(job) => job(),
                    Err(_) => break,
                }
            });
        }
        Self {
            sender,
            runtime,
            pending: Arc::new((Mutex::new(0), std::sync::Condvar::new())),
        }
    }
    pub fn drain(&self) {
        let mut pending = self.pending.0.lock().unwrap();
        while *pending > 0 {
            pending = self.pending.1.wait(pending).unwrap();
        }
    }
    pub fn submit(&self, job: Job) -> std::result::Result<(), Job> {
        let pending = self.pending.clone();
        *pending.0.lock().unwrap() += 1;
        struct Done(Arc<(Mutex<usize>, std::sync::Condvar)>);
        impl Drop for Done {
            fn drop(&mut self) {
                *self.0 .0.lock().unwrap() -= 1;
                self.0 .1.notify_all();
            }
        }
        let done = Done(pending);
        let wrapped: Job = Box::new(move || {
            let _done = done;
            job();
        });
        match self.sender.try_send(wrapped) {
            Ok(()) => {
                self.runtime
                    .metrics
                    .queued
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::TrySendError::Full(job) | mpsc::TrySendError::Disconnected(job)) => {
                self.runtime
                    .metrics
                    .saturated
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err(job)
            }
        }
    }
}
pub fn retry<T>(runtime: &Runtime, mut work: impl FnMut() -> Result<T>) -> Result<T> {
    for _ in 0..8 {
        let epoch = runtime.epoch.load(std::sync::atomic::Ordering::Acquire);
        let result = work();
        if epoch == runtime.epoch.load(std::sync::atomic::Ordering::Acquire) {
            return result;
        }
    }
    Err(std::io::Error::from_raw_os_error(libc::EAGAIN).into())
}

pub fn complete<T>(
    runtime: &Runtime,
    mut work: impl FnMut() -> Result<T>,
    finish: impl FnOnce(Result<T>),
) {
    for _ in 0..8 {
        let epoch = runtime.epoch.load(std::sync::atomic::Ordering::Acquire);
        let result = work();
        let _fence = runtime.reply_fence.read().unwrap();
        if epoch == runtime.epoch.load(std::sync::atomic::Ordering::Acquire) {
            finish(result);
            return;
        }
    }
    finish(Err(std::io::Error::from_raw_os_error(libc::EAGAIN).into()));
}

/// Demand fetches pass queued lookahead. An already-running Git fetch is never
/// interrupted: priority applies at batch boundaries, not within a transfer.
#[derive(Default)]
pub struct FetchGate {
    state: Mutex<(bool, usize)>,
    wake: std::sync::Condvar,
}
impl FetchGate {
    pub fn acquire(&self, speculative: bool) -> FetchPermit<'_> {
        let mut state = self.state.lock().unwrap();
        if !speculative {
            state.1 += 1;
        }
        while state.0 || (speculative && state.1 != 0) {
            state = self.wake.wait(state).unwrap();
        }
        if !speculative {
            state.1 -= 1;
        }
        state.0 = true;
        FetchPermit(self)
    }
}
pub struct FetchPermit<'a>(&'a FetchGate);
impl Drop for FetchPermit<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().0 = false;
        self.0.wake.notify_all();
    }
}

#[cfg(test)]
mod priority_tests {
    use super::*;
    #[test]
    fn demand_precedes_queued_lookahead() {
        let gate = Arc::new(FetchGate::default());
        let held = gate.acquire(true);
        let (sender, receiver) = mpsc::channel();
        std::thread::scope(|scope| {
            let g = gate.clone();
            let s = sender.clone();
            scope.spawn(move || {
                let _permit = g.acquire(true);
                s.send("lookahead").unwrap();
            });
            let g = gate.clone();
            let s = sender.clone();
            scope.spawn(move || {
                let _permit = g.acquire(false);
                s.send("demand").unwrap();
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while gate.state.lock().unwrap().1 == 0 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            drop(held);
            assert_eq!(
                receiver
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap(),
                "demand"
            );
            assert_eq!(
                receiver
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap(),
                "lookahead"
            );
        });
    }
}
