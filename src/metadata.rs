//! Shared metadata runtime. Immutable records are cached; publication invalidates
//! mutable view pointers, never the immutable repository indexes underneath them.
use crate::{db::Store, error, model::*, ops::View, Result};
use rusqlite::{params, OptionalExtension};
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    ops::{Deref, DerefMut},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
pub struct Metrics {
    pub sql_leases: AtomicU64,
    pub sql_writes: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub directory_scans: AtomicU64,
    pub revision_replays: AtomicU64,
    pub metadata_fetches: AtomicU64,
    pub fetch_retries: AtomicU64,
    pub fetch_failures: AtomicU64,
    pub payload_reads: AtomicU64,
    pub invalidations: AtomicU64,
    pub lookups: AtomicU64,
    pub getattrs: AtomicU64,
    pub readdirs: AtomicU64,
    pub readdirplus: AtomicU64,
    pub queued: AtomicU64,
    pub saturated: AtomicU64,
}
struct Record {
    value: Arc<dyn Any + Send + Sync>,
    bytes: usize,
    used: u64,
}
struct Cache {
    items: BTreeMap<String, Record>,
    retired: Vec<Record>,
    bytes: usize,
    clock: u64,
    limit: usize,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            retired: vec![],
            bytes: 0,
            clock: 0,
            limit: 512 * 1024 * 1024,
        }
    }
}
pub type Invalidate = Arc<dyn Fn() -> Result<()> + Send + Sync>;
#[derive(Default)]
pub struct Runtime {
    cache: Mutex<Cache>,
    pub metrics: Metrics,
    pub activity: Arc<crate::activity::Activity>,
    pub epoch: AtomicU64,
    pub stamp: AtomicU64,
    pub managed: AtomicBool,
    pub unhealthy: AtomicBool,
    pub invalidator: Mutex<Option<Invalidate>>,
    pub pool: Mutex<Vec<rusqlite::Connection>>,
    pub pending: AtomicBool,
    pub changed: Mutex<BTreeSet<String>>,
    flush_lock: Mutex<()>,
    pub reply_fence: std::sync::RwLock<()>,
    // Serializes metadata fills for a single key without holding the cache mutex over I/O.
    fills: Mutex<BTreeMap<String, std::sync::Weak<Mutex<()>>>>,
    fetch_gates: Mutex<BTreeMap<String, std::sync::Weak<crate::workers::FetchGate>>>,
}
impl Runtime {
    pub fn get<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        let mut c = self.cache.lock().unwrap();
        c.clock += 1;
        let clock = c.clock;
        let v = c.items.get_mut(key).and_then(|r| {
            r.used = clock;
            r.value.clone().downcast().ok()
        });
        if v.is_some() {
            self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);
        }
        v
    }
    pub fn put<T: Any + Send + Sync>(&self, key: String, value: T, bytes: usize) -> Result<Arc<T>> {
        let mut c = self.cache.lock().unwrap();
        let released: usize = c
            .retired
            .iter()
            .filter(|r| Arc::strong_count(&r.value) == 1)
            .map(|r| r.bytes)
            .sum();
        c.retired.retain(|r| Arc::strong_count(&r.value) > 1);
        c.bytes -= released;
        if let Some(old) = c.items.remove(&key) {
            if Arc::strong_count(&old.value) > 1 {
                c.retired.push(old);
            } else {
                c.bytes -= old.bytes;
            }
        }
        while c.bytes.saturating_add(bytes) > c.limit {
            let victim = c
                .items
                .iter()
                .filter(|(_, r)| Arc::strong_count(&r.value) == 1)
                .min_by_key(|(_, r)| r.used)
                .map(|(k, _)| k.clone());
            let Some(victim) = victim else {
                return Err(std::io::Error::from_raw_os_error(libc::ENOMEM).into());
            };
            let old = c.items.remove(&victim).unwrap();
            c.bytes -= old.bytes;
        }
        let value = Arc::new(value);
        c.clock += 1;
        let used = c.clock;
        c.items.insert(
            key,
            Record {
                value: value.clone(),
                bytes,
                used,
            },
        );
        c.bytes += bytes;
        Ok(value)
    }
    pub fn limit(&self, bytes: usize) {
        self.cache.lock().unwrap().limit = bytes;
    }
    pub fn fill_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut fills = self.fills.lock().unwrap();
        if let Some(lock) = fills.get(key).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        fills.retain(|_, v| v.strong_count() > 0);
        let lock = Arc::new(Mutex::new(()));
        fills.insert(key.into(), Arc::downgrade(&lock));
        lock
    }
    pub fn fetch_gate(&self, repo: &str) -> Arc<crate::workers::FetchGate> {
        let mut gates = self.fetch_gates.lock().unwrap();
        if let Some(gate) = gates.get(repo).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(crate::workers::FetchGate::default());
        gates.insert(repo.into(), Arc::downgrade(&gate));
        gate
    }
    pub fn observe(&self, epoch: u64, stamp: u64) {
        if epoch <= self.epoch.load(Ordering::Acquire) {
            return;
        }
        let _fence = self.reply_fence.write().unwrap();
        let old = self.epoch.fetch_max(epoch, Ordering::AcqRel);
        self.stamp.fetch_max(stamp, Ordering::AcqRel);
        if epoch > old {
            let mut changed = self.changed.lock().unwrap();
            // A publication can affect control aliases and transaction views as
            // well as depot paths. Invalidate all observed dentries until every
            // authoritative mutation supplies a complete dependency set.
            changed.insert(String::new());
            drop(changed);
            self.pending.store(true, Ordering::Release);
            let mut c = self.cache.lock().unwrap();
            let keys: Vec<_> = c
                .items
                .keys()
                .filter(|k| k.starts_with("mutable:") && Arc::strong_count(&c.items[*k].value) == 1)
                .cloned()
                .collect();
            for k in keys {
                let r = c.items.remove(&k).unwrap();
                c.bytes -= r.bytes;
            }
        }
    }
    pub fn flush(&self) -> Result<()> {
        // Concurrent command acknowledgements must wait for the same barrier.
        let _serial = self.flush_lock.lock().unwrap();
        if self.unhealthy.load(Ordering::Acquire) {
            return Err(error(
                "mount unhealthy: committed metadata requires remount",
            ));
        }
        while self.pending.swap(false, Ordering::AcqRel) {
            let callback = self.invalidator.lock().unwrap().clone();
            if let Some(callback) = callback {
                if let Err(e) = callback() {
                    self.unhealthy.store(true, Ordering::Release);
                    return Err(error(format!(
                        "metadata committed but kernel invalidation failed; remount: {e}"
                    )));
                }
            }
        }
        Ok(())
    }
}
// A connection's committed generation is observed before it returns to the pool.
// Triggers cover every authoritative SQL mutation, including transaction candidates.
pub struct Connection {
    conn: Option<rusqlite::Connection>,
    runtime: Arc<Runtime>,
    changes: u64,
}
impl Deref for Connection {
    type Target = rusqlite::Connection;
    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().unwrap()
    }
}
impl DerefMut for Connection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn.as_mut().unwrap()
    }
}
impl Connection {
    pub fn open(root: &Path, runtime: Arc<Runtime>) -> Result<Self> {
        check_io()?;
        runtime.metrics.sql_leases.fetch_add(1, Ordering::Relaxed);
        let existing = runtime.pool.lock().unwrap().pop();
        let conn = match existing {
            Some(c) => c,
            None => {
                let c = rusqlite::Connection::open(root.join("state.sqlite3"))?;
                c.busy_timeout(std::time::Duration::from_secs(30))?;
                c.execute_batch(
                    "PRAGMA foreign_keys=ON;PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;",
                )?;
                c
            }
        };
        let changes = conn.total_changes();
        Ok(Self {
            conn: Some(conn),
            runtime,
            changes,
        })
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let c = self.conn.take().unwrap();
        if c.total_changes() != self.changes && c.is_autocommit() {
            self.runtime
                .metrics
                .sql_writes
                .fetch_add(c.total_changes() - self.changes, Ordering::Relaxed);
            if let Ok((e, s)) = c.query_row("SELECT epoch,stamp FROM metadata_epoch", [], |r| {
                Ok((r.get::<_, u64>(0)?, r.get::<_, u64>(1)?))
            }) {
                self.runtime.observe(e, s);
            }
        }
        let mut pool = self.runtime.pool.lock().unwrap();
        if pool.len() < 16 && c.is_autocommit() {
            pool.push(c);
        }
    }
}
type PersistentOverlay = crate::persistent::Map<(String, String), Option<Entry>>;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ViewState {
    pub map: Map,
    #[serde(with = "overlay_serde")]
    pub overlay: PersistentOverlay,
    pub times: crate::persistent::Map<String, u64>,
    pub stamp: u64,
    pub revision: i64,
    #[serde(default)]
    pub lineage: i64,
    #[serde(default)]
    pub generation_id: String,
    #[serde(skip)]
    pub by_parent: crate::persistent::Map<String, BTreeMap<String, Option<Entry>>>,
}
impl ViewState {
    fn index(&mut self) {
        self.generation_id = format!(
            "{}:{}",
            self.lineage,
            crate::cache::hash(serde_json::to_string(&self.map).unwrap().as_bytes())
        );
        for ((id, rel), entry) in self.overlay.iter() {
            if let Some(m) = self.map.mount.iter().find(|m| &m.id == id) {
                let path = join(&m.path, rel);
                if !self
                    .map
                    .owner(&path)
                    .is_some_and(|(owner, _)| &owner.id == id)
                {
                    continue;
                }
                let (parent, name) = split(&path);
                let parent = parent.to_string();
                if self.by_parent.get(&parent).is_none() {
                    self.by_parent.insert(parent.clone(), BTreeMap::new());
                }
                self.by_parent
                    .get_mut(&parent)
                    .unwrap()
                    .insert(name.into(), entry.clone());
            }
        }
    }
    pub fn generation(&self) -> String {
        self.generation_id.clone()
    }
    pub fn time(&self, path: &str) -> SystemTime {
        UNIX_EPOCH
            + std::time::Duration::from_nanos(
                *self.times.get(&path.to_string()).unwrap_or(&self.stamp),
            )
    }
}
pub fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}
pub fn tree_key(m: &Mount) -> Result<String> {
    let commit = m.commit.as_ref().ok_or_else(|| error("unresolved mount"))?;
    Ok(crate::cache::hash(
        if m.subtree.is_empty() {
            commit.clone()
        } else {
            format!("{commit}:{}", m.subtree)
        }
        .as_bytes(),
    ))
}
impl Store {
    pub fn refresh_epoch(&self) -> Result<()> {
        if !self.runtime.managed.load(Ordering::Acquire) {
            let (e, s) =
                self.conn()?
                    .query_row("SELECT epoch,stamp FROM metadata_epoch", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?;
            self.runtime.observe(e, s);
        }
        Ok(())
    }
    pub fn view_snapshot(&self, v: &View) -> Result<Arc<ViewState>> {
        self.refresh_epoch()?;
        let epoch = self.runtime.epoch.load(Ordering::Acquire);
        let key = format!("mutable:view:{epoch}:{v:?}");
        if let Some(s) = self.runtime.get(&key) {
            return Ok(s);
        }
        let mut state = match v {
            View::Live(n) | View::Base(n) => {
                self.revision_snapshot(self.head(n)?)?.as_ref().clone()
            }
            View::Snapshot(n, i, _) => self
                .revision_snapshot(self.snapshot_revision(n, *i)?.0)?
                .as_ref()
                .clone(),
            View::Transaction(id) => {
                let (m, d, source): (String, String, i64) = self.conn()?.query_row(
                    "SELECT proposal,candidate,source FROM activations WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                let mut state = self.revision_snapshot(source)?.as_ref().clone();
                state.map = serde_json::from_str(&m)?;
                state.overlay = serde_json::from_str::<Vec<Delta>>(&d)?
                    .into_iter()
                    .map(|d| ((d.mount, d.path), d.entry))
                    .collect();
                state
            }
        };
        if matches!(v, View::Base(_) | View::Snapshot(_, _, true)) {
            state.overlay.clear();
            state.times.clear();
        }
        if matches!(
            v,
            View::Base(_) | View::Snapshot(_, _, true) | View::Transaction(_)
        ) {
            state.by_parent.clear();
            state.index();
        }
        let bytes = serde_json::to_vec(&state)?.len() * 3 + 256;
        self.runtime.put(key, state, bytes)
    }
    pub fn revision_snapshot(&self, id: i64) -> Result<Arc<ViewState>> {
        let key = format!("revision:{id}");
        if let Some(s) = self.runtime.get(&key) {
            return Ok(s);
        }
        let mut chain = vec![];
        let mut cursor = Some(id);
        let c = self.conn()?;
        let mut base = None;
        while let Some(revision) = cursor {
            if let Some(state) = self
                .runtime
                .get::<ViewState>(&format!("revision:{revision}"))
            {
                base = Some(state.as_ref().clone());
                break;
            }
            if let Some(data) = c
                .query_row(
                    "SELECT data FROM metadata_checkpoints WHERE revision=?1",
                    [revision],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                let mut state: ViewState = serde_json::from_str(&data)?;
                state.index();
                base = Some(state);
                break;
            }
            let (parent,map,delta,stamp):(Option<i64>,String,String,u64)=c.query_row("SELECT parent,map,delta,COALESCE((SELECT stamp FROM revision_stamps WHERE revision=revisions.id),created*1000000000) FROM revisions WHERE id=?1",[revision],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            chain.push((revision, map, delta, stamp));
            cursor = parent;
            self.runtime
                .metrics
                .revision_replays
                .fetch_add(1, Ordering::Relaxed);
        }
        let mut state = base.unwrap_or(ViewState {
            map: Map::default(),
            overlay: Default::default(),
            times: Default::default(),
            stamp: 0,
            revision: 0,
            lineage: 0,
            generation_id: String::new(),
            by_parent: Default::default(),
        });
        for (revision, map, delta, stamp) in chain.into_iter().rev() {
            if state.revision == 0 {
                state.stamp = stamp;
                state.lineage = revision;
            }
            state.map = serde_json::from_str(&map)?;
            for d in serde_json::from_str::<Vec<Delta>>(&delta)? {
                if let Some(m) = state.map.mount.iter().find(|m| m.id == d.mount) {
                    let p = join(&m.path, &d.path);
                    state.times.insert(p.clone(), stamp);
                    state.times.insert(split(&p).0.into(), stamp);
                    if state
                        .map
                        .owner(&p)
                        .is_some_and(|(owner, _)| owner.id == d.mount)
                    {
                        let (parent, name) = split(&p);
                        let parent = parent.to_string();
                        if state.by_parent.get(&parent).is_none() {
                            state.by_parent.insert(parent.clone(), BTreeMap::new());
                        }
                        state
                            .by_parent
                            .get_mut(&parent)
                            .unwrap()
                            .insert(name.into(), d.entry.clone());
                    }
                }
                state.overlay.insert((d.mount, d.path), d.entry);
            }
            state.revision = revision;
        }
        state.generation_id = format!(
            "{}:{}",
            state.lineage,
            crate::cache::hash(serde_json::to_string(&state.map)?.as_bytes())
        );
        let bytes = serde_json::to_vec(&state)?.len() * 3 + 8192;
        if id % 1024 == 0 {
            c.execute(
                "INSERT OR IGNORE INTO metadata_checkpoints VALUES(?1,?2)",
                params![id, serde_json::to_string(&state)?],
            )?;
        }
        self.runtime.put(key, state, bytes)
    }
    pub fn indexed_directory(
        &self,
        m: &Mount,
        parent: &str,
    ) -> Result<Arc<BTreeMap<String, Entry>>> {
        let tree = tree_key(m)?;
        let key = format!("directory:{}:{tree}:{parent}", m.repo);
        if let Some(v) = self.runtime.get(&key) {
            return Ok(v);
        }
        self.ensure_tree_index(m)?;
        let c = self.conn()?;
        let mut q=c.prepare_cached("SELECT name,entry FROM metadata_entries WHERE repo=?1 AND tree_key=?2 AND parent=?3 ORDER BY name")?;
        let mut entries = BTreeMap::new();
        let mut bytes = 128;
        for row in q.query_map(params![m.repo, tree, parent], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (name, data) = row?;
            bytes += name.len() + data.len() + 160;
            entries.insert(name, serde_json::from_str(&data)?);
        }
        let content: Vec<_> = entries
            .values()
            .filter(|e: &&Entry| e.content.starts_with("git:"))
            .map(|e| e.content.clone())
            .collect();
        if !content.is_empty() {
            let mut q=c.prepare_cached("SELECT content,size FROM metadata_sizes WHERE content IN (SELECT value FROM json_each(?1))")?;
            let found = q
                .query_map([serde_json::to_string(&content)?], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?))
                })?
                .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;
            for id in content {
                let n = found.get(&id).copied();
                self.runtime.put(format!("size:{id}"), n, 128 + id.len())?;
            }
        }
        self.runtime
            .metrics
            .directory_scans
            .fetch_add(1, Ordering::Relaxed);
        self.runtime.put(key, entries, bytes)
    }
    fn indexed_entry(&self, mount: &Mount, parent: &str, name: &str) -> Result<Option<Entry>> {
        let tree = tree_key(mount)?;
        if let Some(entries) = self
            .runtime
            .get::<BTreeMap<String, Entry>>(&format!("directory:{}:{tree}:{parent}", mount.repo))
        {
            return Ok(entries.get(name).cloned());
        }
        let key = format!("entry:{}:{tree}:{parent}/{name}", mount.repo);
        if let Some(entry) = self.runtime.get::<Option<Entry>>(&key) {
            return Ok(entry.as_ref().clone());
        }
        self.ensure_tree_index(mount)?;
        let data: Option<String> = self.conn()?.query_row(
            "SELECT entry FROM metadata_entries WHERE repo=?1 AND tree_key=?2 AND parent=?3 AND name=?4",
            params![mount.repo, tree, parent, name], |r| r.get(0)).optional()?;
        let entry: Option<Entry> = data.as_deref().map(serde_json::from_str).transpose()?;
        self.runtime.put(
            key,
            entry.clone(),
            data.as_ref().map_or(128, |d| d.len() * 3 + 128),
        )?;
        Ok(entry)
    }
    pub fn ensure_tree_index(&self, m: &Mount) -> Result<()> {
        let tree = tree_key(m)?;
        let key = format!("indexed:{}:{tree}", m.repo);
        if self.runtime.get::<bool>(&key).is_some() {
            return Ok(());
        }
        check_io()?;
        let lock = self.runtime.fill_lock(&key);
        let _guard = lock.lock().unwrap();
        let c = self.conn()?;
        let exists: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata_trees WHERE repo=?1 AND tree_key=?2)",
            params![m.repo, tree],
            |r| r.get(0),
        )?;
        drop(c);
        if !exists {
            let entries = self.tree(m)?;
            let mut c = self.conn()?;
            let t = c.transaction()?;
            {
                let mut q=t.prepare_cached("INSERT OR IGNORE INTO metadata_entries(repo,tree_key,parent,name,entry) VALUES(?1,?2,?3,?4,?5)")?;
                for (path, entry) in entries {
                    let (parent, name) = split(&path);
                    q.execute(params![
                        m.repo,
                        tree,
                        parent,
                        name,
                        serde_json::to_string(&entry)?
                    ])?;
                }
            }
            t.execute(
                "INSERT OR IGNORE INTO metadata_trees VALUES(?1,?2)",
                params![m.repo, tree],
            )?;
            t.commit()?;
        }
        self.runtime.put(key, true, 128)?;
        Ok(())
    }
    pub fn lookup_metadata(&self, state: &ViewState, path: &str) -> Result<Entry> {
        if path.is_empty() {
            return Ok(crate::ops::directory());
        }
        if let Some((owner, relative)) = state.map.owner(path) {
            if let Some(e) = state.overlay.get(&(owner.id.clone(), relative.clone())) {
                return e.clone().ok_or_else(|| error("not found"));
            }
            if relative.is_empty() {
                return Ok(crate::ops::directory());
            }
            let (parent, name) = split(&relative);
            if let Some(e) = self.indexed_entry(owner, parent, name)? {
                return Ok(e);
            }
        }
        if state
            .map
            .mount
            .iter()
            .any(|m| m.path.starts_with(&format!("{path}/")))
        {
            return Ok(crate::ops::directory());
        }
        Err(error("not found"))
    }
    pub fn directory_metadata(
        &self,
        state: &ViewState,
        path: &str,
    ) -> Result<Arc<BTreeMap<String, Entry>>> {
        if self.lookup_metadata(state, path)?.kind != Kind::Directory {
            return Err(error("not a directory"));
        }
        let mut names = BTreeSet::new();
        if let Some((owner, relative)) = state.map.owner(path) {
            names.extend(self.indexed_directory(owner, &relative)?.keys().cloned());
        }
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        for m in &state.map.mount {
            if let Some(tail) = m.path.strip_prefix(&prefix) {
                if let Some(n) = tail.split('/').next().filter(|n| !n.is_empty()) {
                    names.insert(n.to_string());
                }
            }
        }
        if let Some(edits) = state.by_parent.get(&path.to_string()) {
            for (name, entry) in edits {
                if entry.is_some() {
                    names.insert(name.clone());
                } else {
                    names.remove(name);
                }
            }
        }
        let mut entries = BTreeMap::new();
        for name in names {
            entries.insert(
                name.clone(),
                self.lookup_metadata(state, &join(path, &name))?,
            );
        }
        Ok(Arc::new(entries))
    }
    pub fn known_size(&self, e: &Entry) -> Result<Option<u64>> {
        if e.kind == Kind::Directory || !e.content.starts_with("git:") {
            return Ok(Some(e.size));
        }
        let key = format!("size:{}", e.content);
        if let Some(n) = self.runtime.get::<Option<u64>>(&key) {
            return Ok(*n);
        }
        let value = self
            .conn()?
            .query_row(
                "SELECT size FROM metadata_sizes WHERE content=?1",
                [&e.content],
                |r| r.get(0),
            )
            .optional()?;
        self.runtime.put(key, value, 128 + e.content.len())?;
        Ok(value)
    }
    pub fn remember_size(&self, e: &Entry, size: u64) -> Result<()> {
        if e.content.starts_with("git:") {
            self.conn()?.execute(
                "INSERT OR REPLACE INTO metadata_sizes VALUES(?1,?2)",
                params![e.content, size],
            )?;
            self.runtime.put(
                format!("size:{}", e.content),
                Some(size),
                128 + e.content.len(),
            )?;
        }
        Ok(())
    }
    fn traversal_key(view: &View, revision: i64, path: &str) -> String {
        format!("mutable:traversal:{view:?}:{revision}:{path}")
    }
    /// Listing records intent only: it never starts a fetch.
    pub fn note_listing(&self, view: &View, state: &ViewState, path: &str) -> Result<()> {
        let key = Self::traversal_key(view, state.revision, path);
        if self.runtime.get::<bool>(&key).is_none() {
            self.runtime.put(key, true, 256 + path.len())?;
        }
        Ok(())
    }
    pub fn traversal_size(&self, view: &View, path: &str, entry: &Entry) -> Result<u64> {
        if let Some(size) = self.known_size(entry)? {
            return Ok(size);
        }
        check_io()?;
        let state = self.view_snapshot(view)?;
        let (parent, name) = split(path);
        let key = Self::traversal_key(view, state.revision, parent);
        if !self
            .runtime
            .get::<bool>(&key)
            .is_some_and(|enabled| *enabled)
        {
            return self.exact_size(entry);
        }
        let lock = self.runtime.fill_lock(&key);
        let _guard = lock.lock().unwrap();
        if let Some(size) = self.known_size(entry)? {
            return Ok(size);
        }
        if !self
            .runtime
            .get::<bool>(&key)
            .is_some_and(|enabled| *enabled)
        {
            return self.exact_size(entry);
        }
        // Parent and child listings followed by stat are evidence of a recursive
        // scan. Names-only recursion still never fetches anything.
        let recursive = !parent.is_empty()
            && self
                .runtime
                .get::<bool>(&Self::traversal_key(view, state.revision, split(parent).0))
                .is_some_and(|enabled| *enabled);
        let result = (|| -> Result<()> {
            let limit = if recursive { 1024 } else { 256 };
            let owner = state.map.owner(path).map(|(m, _)| m.id.clone());
            let repo = entry.content.rsplit_once(':').map(|(prefix, _)| prefix);
            let mut batch = vec![entry.clone()];
            let mut seen = BTreeSet::from([entry.content.clone()]);
            let current = self.directory_metadata(&state, parent)?;
            let mut directories = vec![parent.to_string()];
            if recursive {
                // Only one level: immediate children and siblings, within this
                // mount. Enumerating these uses the already-indexed owner tree.
                for (name, child) in current.iter() {
                    if directories.len() >= 32 {
                        break;
                    }
                    let path = join(parent, name);
                    if child.kind == Kind::Directory
                        && state.map.owner(&path).map(|(m, _)| &m.id) == owner.as_ref()
                    {
                        directories.push(path);
                    }
                }
                let (ancestor, own_name) = split(parent);
                let siblings = self.directory_metadata(&state, ancestor)?;
                for (name, child) in siblings
                    .range(own_name.to_string()..)
                    .chain(siblings.range(..own_name.to_string()))
                {
                    if directories.len() >= 32 {
                        break;
                    }
                    let path = join(ancestor, name);
                    if path != parent
                        && child.kind == Kind::Directory
                        && state.map.owner(&path).map(|(m, _)| &m.id) == owner.as_ref()
                    {
                        directories.push(path);
                    }
                }
            }
            for directory in directories {
                if batch.len() >= limit {
                    break;
                }
                let entries = if directory == parent {
                    current.clone()
                } else {
                    self.directory_metadata(&state, &directory)?
                };
                let start = if directory == parent { name } else { "" };
                for (_, sibling) in entries
                    .range(start.to_string()..)
                    .chain(entries.range(..start.to_string()))
                {
                    if batch.len() >= limit {
                        break;
                    }
                    if sibling.kind == Kind::Directory
                        || sibling.content.rsplit_once(':').map(|(prefix, _)| prefix) != repo
                        || !seen.insert(sibling.content.clone())
                    {
                        continue;
                    }
                    if self.known_size(sibling)?.is_none() {
                        batch.push(sibling.clone());
                    }
                }
            }
            self.resolve_sizes_with_priority(&batch, true)
        })();
        if let Err(e) = result {
            let span = crate::cache::Span::new(&self.root, "metadata-prefetch");
            span.event(
                "failed",
                Some(&format!(
                    "{view:?}:{parent}: {e}; falling back to requested file"
                )),
            );
            // A broken speculative sibling must not make an accessible requested
            // file fail. Disable speculation for this directory revision after any failure.
            self.runtime.put(key, false, 256 + parent.len())?;
        }
        self.exact_size(entry)
    }
    pub fn exact_size(&self, e: &Entry) -> Result<u64> {
        if let Some(n) = self.known_size(e)? {
            return Ok(n);
        }
        check_io()?;
        self.resolve_sizes(std::slice::from_ref(e))?;
        self.known_size(e)?
            .ok_or_else(|| error("size unavailable after fetch"))
    }

    pub fn metrics(&self) -> serde_json::Value {
        let m = &self.runtime.metrics;
        let c = self.runtime.cache.lock().unwrap();
        macro_rules! n {
            ($x:ident) => {
                m.$x.load(Ordering::Relaxed)
            };
        }
        serde_json::json!({"sql_leases":n!(sql_leases),"sql_writes":n!(sql_writes),"cache_hits":n!(cache_hits),"cache_misses":n!(cache_misses),"directory_scans":n!(directory_scans),"revision_replays":n!(revision_replays),"metadata_fetches":n!(metadata_fetches),"fetch_retries":n!(fetch_retries),"fetch_failures":n!(fetch_failures),"payload_reads":n!(payload_reads),"invalidations":n!(invalidations),"lookups":n!(lookups),"getattrs":n!(getattrs),"readdirs":n!(readdirs),"readdirplus":n!(readdirplus),"queued":n!(queued),"saturated":n!(saturated),"cache_bytes":c.bytes,"cache_limit":c.limit,"git_subprocesses":crate::backend::GIT_CALLS.load(Ordering::Relaxed)})
    }
}
pub fn logical_size(bytes: &[u8]) -> Result<u64> {
    if !bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
        return Ok(bytes.len() as u64);
    }
    let text = std::str::from_utf8(bytes)?;
    let oid = text
        .lines()
        .find_map(|l| l.strip_prefix("oid sha256:"))
        .ok_or_else(|| error("invalid LFS pointer"))?;
    if oid.len() != 64 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(error("invalid LFS pointer digest"));
    }
    text.lines()
        .find_map(|l| l.strip_prefix("size "))
        .ok_or_else(|| error("invalid LFS pointer size"))?
        .parse()
        .map_err(Into::into)
}

mod overlay_serde {
    use super::*;
    use serde::{Deserialize, Serialize};
    pub fn serialize<S: serde::Serializer>(
        o: &PersistentOverlay,
        s: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        o.iter()
            .map(|((m, p), e)| Delta {
                mount: m.clone(),
                path: p.clone(),
                entry: e.clone(),
            })
            .collect::<Vec<_>>()
            .serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<PersistentOverlay, D::Error> {
        Ok(Vec::<Delta>::deserialize(d)?
            .into_iter()
            .map(|d| ((d.mount, d.path), d.entry))
            .collect())
    }
}

thread_local! { static CACHE_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub fn check_io() -> Result<()> {
    if CACHE_ONLY.get() {
        Err(std::io::Error::from_raw_os_error(libc::EAGAIN).into())
    } else {
        Ok(())
    }
}
pub fn cached_only<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            CACHE_ONLY.set(self.0);
        }
    }
    let _reset = Reset(CACHE_ONLY.replace(true));
    f()
}
pub fn would_block(e: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.raw_os_error() == Some(libc::EAGAIN))
}
impl Store {
    pub fn observe_committed(&self) -> Result<()> {
        let (epoch, stamp) =
            self.conn()?
                .query_row("SELECT epoch,stamp FROM metadata_epoch", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        self.runtime.observe(epoch, stamp);
        Ok(())
    }
    pub fn cached_value<T: Any + Send + Sync + serde::Serialize>(
        &self,
        key: &str,
        load: impl FnOnce() -> Result<T>,
    ) -> Result<Arc<T>> {
        self.refresh_epoch()?;
        let key = format!(
            "mutable:{}:{key}",
            self.runtime.epoch.load(Ordering::Acquire)
        );
        if let Some(v) = self.runtime.get(&key) {
            return Ok(v);
        }
        let value = load()?;
        let bytes = serde_json::to_vec(&value)?.len() * 2 + 128;
        self.runtime.put(key, value, bytes)
    }
    pub fn warmup(
        &self,
        workspace: &str,
        paths: &[String],
        recursive: bool,
    ) -> Result<serde_json::Value> {
        let state = self.view_snapshot(&View::Live(workspace.into()))?;
        let mut pending = if paths.is_empty() {
            vec![String::new()]
        } else {
            paths.to_vec()
        };
        for p in &pending {
            if !path_ok(p) {
                return Err(error("warmup paths must be depot-relative without . or .."));
            }
        }
        let mut seen = BTreeSet::new();
        let mut directories = 0;
        let mut files = 0;
        let mut failures = vec![];
        while let Some(path) = pending.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let result = (|| -> Result<()> {
                let entry = self.lookup_metadata(&state, &path)?;
                if entry.kind == Kind::Directory {
                    directories += 1;
                    let entries = self.directory_metadata(&state, &path)?;
                    let mut batch = vec![];
                    for (name, e) in entries.iter() {
                        if e.kind == Kind::Directory {
                            if recursive {
                                pending.push(join(&path, name));
                            }
                        } else {
                            batch.push(e.clone());
                            files += 1;
                        }
                    }
                    self.resolve_sizes(&batch)?;
                } else {
                    self.exact_size(&entry)?;
                    files += 1;
                }
                Ok(())
            })();
            if let Err(e) = result {
                failures.push(serde_json::json!({"path":path,"error":e.to_string()}));
            }
            (self.progress)(&format!(
                "warmup {}: {directories} directories, {files} files, {} failures",
                if path.is_empty() { "/" } else { &path },
                failures.len()
            ));
        }
        Ok(
            serde_json::json!({"revision":state.revision,"directories":directories,"files":files,"failures":failures}),
        )
    }
    pub fn resolve_sizes(&self, entries: &[Entry]) -> Result<()> {
        self.resolve_sizes_with_priority(entries, false)
    }
    fn resolve_sizes_with_priority(&self, entries: &[Entry], speculative: bool) -> Result<()> {
        let mut repos = BTreeMap::<String, BTreeMap<String, Entry>>::new();
        for e in entries {
            if self.known_size(e)?.is_some() {
                continue;
            }
            let (repo, oid) = e
                .content
                .strip_prefix("git:")
                .and_then(|s| s.split_once(':'))
                .ok_or_else(|| error("invalid object"))?;
            repos
                .entry(repo.into())
                .or_default()
                .insert(oid.into(), e.clone());
        }
        for (repo, objects) in repos {
            let entries: Vec<_> = objects.into_iter().collect();
            for chunk in entries.chunks(1024) {
                let mut pending = vec![];
                for (oid, entry) in chunk {
                    if self.known_size(entry)?.is_none() {
                        pending.push((oid, entry));
                    }
                }
                if pending.is_empty() {
                    continue;
                }
                let oids: Vec<_> = pending.iter().map(|(oid, _)| (*oid).clone()).collect();
                let sizes = self.ensure_blobs_with_priority(&repo, &oids, speculative)?;
                let mut small = vec![];
                for ((oid, e), size) in pending.iter().zip(sizes) {
                    // Canonical Git LFS pointers are small text objects. Reading these
                    // identifies their logical length without invoking LFS smudge.
                    if size <= 1024 {
                        small.push((*oid, *e));
                    } else {
                        self.remember_size(e, size)?;
                    }
                }
                if !small.is_empty() {
                    let input = small
                        .iter()
                        .map(|(oid, _)| oid.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n";
                    let bytes = crate::backend::run_git(
                        &self.repo_path(&repo)?,
                        &["cat-file", "--batch"],
                        Some(input.as_bytes()),
                        None,
                        true,
                    )?;
                    let mut offset = 0;
                    for (oid, e) in small {
                        let end = bytes
                            .get(offset..)
                            .ok_or_else(|| error("truncated batch header"))?
                            .iter()
                            .position(|b| *b == b'\n')
                            .ok_or_else(|| error("invalid batch header"))?
                            + offset;
                        let header = std::str::from_utf8(&bytes[offset..end])?;
                        let fields: Vec<_> = header.split_whitespace().collect();
                        if fields.len() != 3 || fields[0] != oid || fields[1] != "blob" {
                            return Err(error("invalid batch object"));
                        }
                        let size: usize = fields[2].parse()?;
                        offset = end + 1;
                        let end = offset
                            .checked_add(size)
                            .ok_or_else(|| error("invalid batch size"))?;
                        let body = bytes
                            .get(offset..end)
                            .ok_or_else(|| error("truncated batch object"))?;
                        if bytes.get(end) != Some(&b'\n') {
                            return Err(error("missing batch object terminator"));
                        }
                        self.remember_size(e, logical_size(body)?)?;
                        offset = end + 1;
                    }
                }
            }
        }
        Ok(())
    }
}
