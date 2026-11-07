//! Reply ownership is independent of worker lifetime. Cancellation never waits
//! for backend work and only the winner may publish kernel-visible resources.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

type Cancel = Box<dyn FnOnce() + Send>;
#[derive(Clone, Default)]
pub(crate) struct PendingRequests(Arc<Mutex<HashMap<u64, Cancel>>>);
pub(crate) trait InterruptibleReply: Send + 'static {
    fn interrupted(self);
}
macro_rules! replies {
    ($($t:ty),*) => {$(impl InterruptibleReply for $t {
        fn interrupted(self) { self.error(libc::EINTR); }
    })*};
}
replies!(
    fuser::ReplyEntry,
    fuser::ReplyAttr,
    fuser::ReplyOpen,
    fuser::ReplyDirectory,
    fuser::ReplyDirectoryPlus,
    fuser::ReplyData,
    fuser::ReplyXattr
);

impl PendingRequests {
    pub fn register<R: InterruptibleReply>(&self, id: u64, reply: R) -> Pending<R> {
        let slot = Arc::new(Mutex::new(Some(reply)));
        let cancel = slot.clone();
        let previous = self.0.lock().unwrap().insert(
            id,
            Box::new(move || {
                let reply = cancel.lock().unwrap().take();
                if let Some(reply) = reply {
                    reply.interrupted();
                }
            }),
        );
        assert!(previous.is_none(), "duplicate FUSE request ID");
        Pending {
            id,
            registry: self.clone(),
            slot,
        }
    }
    pub fn interrupt(&self, id: u64) -> bool {
        let cancel = self.0.lock().unwrap().remove(&id);
        if let Some(cancel) = cancel {
            cancel();
            true
        } else {
            false
        }
    }
}
pub(crate) struct Pending<R> {
    id: u64,
    registry: PendingRequests,
    slot: Arc<Mutex<Option<R>>>,
}
impl<R> Pending<R> {
    pub fn canceled(&self) -> bool {
        self.slot.lock().unwrap().is_none()
    }
    pub fn complete(self, finish: impl FnOnce(R)) {
        // Remove registration before claiming the reply: completion has won if
        // an interrupt arrives after this point. No locks cover finish/backend IO.
        self.registry.0.lock().unwrap().remove(&self.id);
        let reply = self.slot.lock().unwrap().take();
        if let Some(reply) = reply {
            finish(reply);
        }
    }
}
impl<R> Drop for Pending<R> {
    fn drop(&mut self) {
        self.registry.0.lock().unwrap().remove(&self.id);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Reply(Arc<AtomicUsize>);
    impl InterruptibleReply for Reply {
        fn interrupted(self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[test]
    fn interruption_claims_reply_without_waiting_for_work() {
        let registry = PendingRequests::default();
        let canceled = Arc::new(AtomicUsize::new(0));
        let pending = registry.register(1, Reply(canceled.clone()));
        assert!(registry.interrupt(1));
        assert!(pending.canceled());
        pending.complete(|_| panic!("canceled request published a handle or lookup reference"));
        assert_eq!(canceled.load(Ordering::SeqCst), 1);
        assert!(!registry.interrupt(1));
        assert!(registry.0.lock().unwrap().is_empty());
    }
    #[test]
    fn completion_interrupt_race_has_exactly_one_winner() {
        let registry = PendingRequests::default();
        for id in 0..500 {
            let count = Arc::new(AtomicUsize::new(0));
            let pending = registry.register(id, Reply(count.clone()));
            std::thread::scope(|s| {
                s.spawn(|| {
                    registry.interrupt(id);
                });
                s.spawn(move || {
                    pending.complete(|r| {
                        r.0.fetch_add(1, Ordering::SeqCst);
                    })
                });
            });
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
        assert!(registry.0.lock().unwrap().is_empty());
    }
    #[test]
    fn abandoned_worker_removes_registration() {
        let registry = PendingRequests::default();
        drop(registry.register(1, Reply(Arc::new(AtomicUsize::new(0)))));
        assert!(!registry.interrupt(1));
    }
}
