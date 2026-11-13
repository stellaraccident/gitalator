//! Immutable object metadata shared by all workspaces in a store. Attribute
//! resolution fetches Git blobs, but never extracts files or smudges LFS.
use crate::{activity, error, git, util::Lock, Result};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

pub const DIRECTORY_BATCH: usize = 256;
pub const TRAVERSAL_BATCH: usize = 1024;
pub const TRAVERSAL_DIRS: usize = 32;
const SMALL_BLOB: u64 = 1024;

#[derive(Clone)]
pub struct BlobInfo {
    pub size: u64,
    pub small: Option<Arc<[u8]>>,
}
impl BlobInfo {
    pub fn logical_size(&self) -> u64 {
        self.small
            .as_deref()
            .and_then(lfs_pointer)
            .map(|p| p.size)
            .unwrap_or(self.size)
    }
}
pub struct LfsPointer<'a> {
    pub oid: &'a str,
    pub size: u64,
}
pub fn lfs_pointer(bytes: &[u8]) -> Option<LfsPointer<'_>> {
    if bytes.len() > SMALL_BLOB as usize {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.starts_with("version https://git-lfs.github.com/spec/v1\n") {
        return None;
    }
    let oid = text
        .lines()
        .find_map(|line| line.strip_prefix("oid sha256:"))?;
    if oid.len() != 64 || !oid.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let size = text
        .lines()
        .find_map(|line| line.strip_prefix("size "))?
        .parse()
        .ok()?;
    Some(LfsPointer { oid, size })
}

#[derive(Default)]
pub struct Metadata {
    repos: Mutex<HashMap<String, Arc<Repo>>>,
    batches: AtomicU64,
}
#[derive(Default)]
struct Repo {
    info: Mutex<HashMap<String, BlobInfo>>,
    gate: Gate,
}
impl Metadata {
    fn repo(&self, repo: &str) -> Arc<Repo> {
        self.repos
            .lock()
            .unwrap()
            .entry(repo.into())
            .or_default()
            .clone()
    }
    pub fn get(&self, repo: &str, oid: &str) -> Option<BlobInfo> {
        self.repo(repo).info.lock().unwrap().get(oid).cloned()
    }
    pub fn batches(&self) -> u64 {
        self.batches.load(Ordering::Relaxed)
    }
    pub fn resolve(
        &self,
        root: &Path,
        repo: &str,
        oids: &[String],
        speculative: bool,
    ) -> Result<()> {
        let state = self.repo(repo);
        let _gate = state.gate.acquire(speculative)?;
        let pending: Vec<_> = {
            let info = state.info.lock().unwrap();
            oids.iter()
                .filter(|id| !info.contains_key(*id))
                .cloned()
                .collect()
        };
        if pending.is_empty() {
            return Ok(());
        }
        let _lock = Lock::take(&root.join("locks").join(repo))?;
        activity::check()?;
        let started = std::time::Instant::now();
        self.batches.fetch_add(1, Ordering::Relaxed);
        activity::event(
            "info",
            &format!(
                "Attribute batch: {} objects, {}, repository {repo}",
                pending.len(),
                if speculative { "lookahead" } else { "demand" }
            ),
        );
        let path = root.join("repos").join(repo);
        let sizes = git::ensure_blobs(&path, &pending)?;
        let small: Vec<_> = pending
            .iter()
            .zip(&sizes)
            .filter(|(_, size)| **size <= SMALL_BLOB)
            .map(|(id, _)| id.clone())
            .collect();
        let mut bodies = read_small(&path, &small)?;
        let mut info = state.info.lock().unwrap();
        for (oid, size) in pending.into_iter().zip(sizes) {
            info.insert(
                oid.clone(),
                BlobInfo {
                    size,
                    small: bodies.remove(&oid),
                },
            );
        }
        activity::event(
            "info",
            &format!(
                "Attribute batch resolved in {:.3}s; {} small blobs inspected",
                started.elapsed().as_secs_f64(),
                small.len()
            ),
        );
        Ok(())
    }
}

fn read_small(repo: &Path, oids: &[String]) -> Result<HashMap<String, Arc<[u8]>>> {
    let mut result = HashMap::new();
    if oids.is_empty() {
        return Ok(result);
    }
    let input = oids.join("\n") + "\n";
    let bytes = git::local(repo, &["cat-file", "--batch"], Some(input.as_bytes()))?;
    let mut offset = 0;
    for oid in oids {
        activity::check()?;
        let tail = bytes.get(offset..).ok_or("truncated batch response")?;
        let end = offset
            + tail
                .iter()
                .position(|b| *b == b'\n')
                .ok_or("missing batch header")?;
        let header = std::str::from_utf8(&bytes[offset..end])?;
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() != 3 || fields[0] != oid || fields[1] != "blob" {
            return Err(error("invalid batch header"));
        }
        let size: usize = fields[2].parse()?;
        if size > SMALL_BLOB as usize {
            return Err(error("unexpected oversized metadata blob"));
        }
        offset = end + 1;
        let end = offset.checked_add(size).ok_or("batch size overflow")?;
        let body = bytes.get(offset..end).ok_or("truncated batch body")?;
        if bytes.get(end) != Some(&b'\n') {
            return Err(error("missing batch terminator"));
        }
        result.insert(oid.clone(), Arc::from(body));
        offset = end + 1;
    }
    if offset != bytes.len() {
        return Err(error("extra batch response"));
    }
    Ok(result)
}

#[derive(Default)]
pub(crate) struct Gate {
    state: Mutex<GateState>,
    ready: Condvar,
}
#[derive(Default)]
struct GateState {
    busy: bool,
    demand: usize,
}
pub(crate) struct Permit<'a>(&'a Gate);
impl Gate {
    pub(crate) fn acquire(&self, speculative: bool) -> Result<Permit<'_>> {
        let mut state = self.state.lock().unwrap();
        if !speculative {
            state.demand += 1;
        }
        loop {
            if let Err(e) = activity::check() {
                if !speculative {
                    state.demand -= 1;
                }
                self.ready.notify_all();
                return Err(e);
            }
            if !state.busy && (!speculative || state.demand == 0) {
                state.busy = true;
                if !speculative {
                    state.demand -= 1;
                }
                return Ok(Permit(self));
            }
            state = self
                .ready
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap()
                .0;
        }
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().busy = false;
        self.0.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn demand_precedes_waiting_lookahead_and_cancel_releases_waiter() {
        let gate = Arc::new(Gate::default());
        let held = gate.acquire(false).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let g = gate.clone();
        let t = tx.clone();
        let lookahead = std::thread::spawn(move || {
            let _permit = g.acquire(true).unwrap();
            t.send("lookahead").unwrap();
        });
        let g = gate.clone();
        let demand = std::thread::spawn(move || {
            let _permit = g.acquire(false).unwrap();
            tx.send("demand").unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while gate.state.lock().unwrap().demand == 0 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(held);
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "demand");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "lookahead"
        );
        lookahead.join().unwrap();
        demand.join().unwrap();

        let held = gate.acquire(true).unwrap();
        let task = Arc::new(activity::Task::new("cancel gate"));
        let worker = task.clone();
        let g = gate.clone();
        let waiter = std::thread::spawn(move || {
            let _context = worker.enter();
            g.acquire(false).err().unwrap()
        });
        task.cancel();
        assert!(activity::interrupted(&*waiter.join().unwrap()));
        assert_eq!(gate.state.lock().unwrap().demand, 0);
        drop(held);
        let _permit = gate.acquire(true).unwrap();
    }
}
