use crate::{
    db::Store,
    error, metadata,
    model::{Entry, Kind},
    ops::{directory, View},
    Result,
};
use fuser::*;
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, PartialEq, Eq)]
enum Node {
    Dir,
    Control(String, Control),
    Source(View, String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Control {
    Draft,
    Active,
    Status,
    SnapshotDraft(i64),
    SnapshotActive(i64),
    TxStatus,
    TxMap,
    TxConflicts,
}
#[derive(Clone)]
struct Handle {
    node: Node,
    entry: Entry,
    bytes: Option<Vec<u8>>,
    generation: Option<String>,
}
#[derive(Default)]
struct Inodes {
    paths: BTreeMap<u64, String>,
    ids: BTreeMap<String, u64>,
    next: u64,
    lineages: BTreeMap<u64, String>,
    saved: BTreeMap<u64, (Entry, SystemTime)>,
    references: BTreeMap<u64, u64>,
    opens: BTreeMap<u64, u64>,
    detached: BTreeMap<u64, (Entry, SystemTime)>,
}
impl Inodes {
    fn collect(&mut self, ino: u64) {
        if ino == 1
            || self.references.get(&ino).copied().unwrap_or(0) != 0
            || self.opens.get(&ino).copied().unwrap_or(0) != 0
        {
            return;
        }
        if let Some(path) = self.paths.remove(&ino) {
            if self.ids.get(&path) == Some(&ino) {
                self.ids.remove(&path);
            }
        }
        self.references.remove(&ino);
        self.opens.remove(&ino);
        self.detached.remove(&ino);
        self.lineages.remove(&ino);
        self.saved.remove(&ino);
    }
}
#[derive(Clone)]
struct DirectoryEntry {
    name: String,
    entry: Entry,
    time: SystemTime,
    generation: String,
}
#[derive(Clone)]
struct DirectoryHandle {
    path: String,
    epoch: u64,
    entries: Arc<Vec<DirectoryEntry>>,
}
pub struct Fs {
    store: Store,
    inodes: Arc<Mutex<Inodes>>,
    handles: BTreeMap<u64, Handle>,
    next_handle: u64,
    file_inodes: BTreeMap<u64, u64>,
    temps: BTreeMap<String, Vec<u8>>,
    directories: Arc<Mutex<BTreeMap<u64, DirectoryHandle>>>,
    next_directory: Arc<AtomicU64>,
    workers: Arc<crate::workers::Workers>,
    ttl: Arc<AtomicU64>,
}
impl Fs {
    pub fn new(store: Store) -> Self {
        store.runtime.managed.store(true, Ordering::Release);
        let workers = Arc::new(crate::workers::Workers::new(store.runtime.clone()));
        Self {
            store,
            inodes: Arc::new(Mutex::new(Inodes {
                paths: BTreeMap::from([(1, String::new())]),
                ids: BTreeMap::from([(String::new(), 1)]),
                next: 2,
                ..Default::default()
            })),
            handles: BTreeMap::new(),
            next_handle: 1,
            file_inodes: BTreeMap::new(),
            temps: BTreeMap::new(),
            directories: Default::default(),
            next_directory: Arc::new(AtomicU64::new(1)),
            workers,
            ttl: Arc::new(AtomicU64::new(0)),
        }
    }
    fn reader(&self) -> Self {
        Self {
            store: self.store.clone(),
            inodes: self.inodes.clone(),
            handles: BTreeMap::new(),
            next_handle: 1,
            file_inodes: BTreeMap::new(),
            temps: self.temps.clone(),
            directories: self.directories.clone(),
            next_directory: self.next_directory.clone(),
            workers: self.workers.clone(),
            ttl: self.ttl.clone(),
        }
    }
    fn inode(&self, p: &str) -> u64 {
        let generation = match self.node(p) {
            Ok(Node::Source(v, _)) => self
                .store
                .view_snapshot(&v)
                .map(|s| s.generation())
                .unwrap_or_default(),
            _ => String::new(),
        };
        self.inode_key(p, &generation)
    }
    fn inode_key(&self, p: &str, generation: &str) -> u64 {
        let mut i = self.inodes.lock().unwrap();
        if let Some(ino) = i.ids.get(p).copied() {
            if i.lineages.get(&ino).map(String::as_str).unwrap_or("") == generation {
                return ino;
            }
            if let Some(saved) = i.saved.get(&ino).cloned() {
                i.detached.insert(ino, saved);
            }
        }
        let ino = i.next;
        i.next += 1;
        i.ids.insert(p.into(), ino);
        i.paths.insert(ino, p.into());
        i.lineages.insert(ino, generation.into());
        ino
    }
    fn reference(&self, ino: u64) {
        *self
            .inodes
            .lock()
            .unwrap()
            .references
            .entry(ino)
            .or_default() += 1;
    }
    fn path(&self, ino: u64) -> Result<String> {
        self.inodes
            .lock()
            .unwrap()
            .paths
            .get(&ino)
            .cloned()
            .ok_or_else(|| error("not found"))
    }
    fn ttl(&self, p: &str) -> Duration {
        if p.starts_with("workspaces/") && !p.contains("/depot") && !p.contains("/READONLY") {
            Duration::ZERO
        } else {
            Duration::from_secs(self.ttl.load(Ordering::Acquire))
        }
    }
    fn enqueue(&self, job: impl FnOnce(bool) + Send + 'static) {
        // Keep the reply on a shared slot so rejection can return EAGAIN without
        // running blocking work on the dispatch thread.
        let job = Arc::new(Mutex::new(Some(job)));
        let queued = job.clone();
        if self
            .workers
            .submit(Box::new(move || {
                if let Some(f) = queued.lock().unwrap().take() {
                    f(true);
                }
            }))
            .is_err()
        {
            if let Some(f) = job.lock().unwrap().take() {
                f(false);
            }
        }
    }
    fn node(&self, p: &str) -> Result<Node> {
        let parts: Vec<_> = p.split('/').filter(|x| !x.is_empty()).collect();
        match parts.as_slice() {
            [] | ["workspaces"] | ["snapshots"] | ["transactions"] => Ok(Node::Dir),
            ["workspaces", n] => {
                self.store.head(n)?;
                Ok(Node::Dir)
            }
            ["snapshots", n] => {
                self.store.head(n)?;
                Ok(Node::Dir)
            }
            ["snapshots", n, num] => {
                self.store.snapshot_revision(n, num.parse()?)?;
                Ok(Node::Dir)
            }
            ["transactions", id] => {
                self.store.conflict_list(id)?;
                Ok(Node::Dir)
            }
            ["workspaces", n, "VERSION_MAP"] => Ok(Node::Control((*n).into(), Control::Draft)),
            ["workspaces", n, "ACTIVE_VERSION_MAP"] => {
                Ok(Node::Control((*n).into(), Control::Active))
            }
            ["workspaces", n, "STATUS"] => Ok(Node::Control((*n).into(), Control::Status)),
            ["workspaces", n, surface, rest @ ..]
                if *surface == "depot" || *surface == "READONLY" =>
            {
                Ok(Node::Source(
                    if *surface == "depot" {
                        View::Live((*n).into())
                    } else {
                        View::Base((*n).into())
                    },
                    rest.join("/"),
                ))
            }
            ["snapshots", n, num, "VERSION_MAP"] => Ok(Node::Control(
                (*n).into(),
                Control::SnapshotDraft(num.parse()?),
            )),
            ["snapshots", n, num, "ACTIVE_VERSION_MAP"] => Ok(Node::Control(
                (*n).into(),
                Control::SnapshotActive(num.parse()?),
            )),
            ["snapshots", n, num, surface, rest @ ..]
                if *surface == "depot" || *surface == "READONLY" =>
            {
                Ok(Node::Source(
                    View::Snapshot((*n).into(), num.parse()?, *surface == "READONLY"),
                    rest.join("/"),
                ))
            }
            ["transactions", id, "depot", rest @ ..] => Ok(Node::Source(
                View::Transaction((*id).into()),
                rest.join("/"),
            )),
            ["transactions", id, "STATUS"] => Ok(Node::Control((*id).into(), Control::TxStatus)),
            ["transactions", id, "PROPOSED_VERSION_MAP"] => {
                Ok(Node::Control((*id).into(), Control::TxMap))
            }
            ["transactions", id, "conflicts.json"] => {
                Ok(Node::Control((*id).into(), Control::TxConflicts))
            }
            _ => Err(error("not found")),
        }
    }
    fn control(&self, n: &str, c: &Control) -> Result<Vec<u8>> {
        let key = format!("control:{n}:{c:?}");
        let result =
            self.store.cached_value(&key, || {
                let s = match c {
            Control::Draft => self.store.draft(n)?,
            Control::Active => toml::to_string_pretty(&self.store.state(self.store.head(n)?)?.0)?,
            Control::SnapshotDraft(i) => self.store.snapshot_revision(n, *i)?.1,
            Control::SnapshotActive(i) => toml::to_string_pretty(
                &self.store.state(self.store.snapshot_revision(n, *i)?.0)?.0,
            )?,
            Control::Status => {
                serde_json::json!({"head":self.store.head(n)?,"frozen":self.store.frozen(n)?})
                    .to_string()
            }
            Control::TxConflicts => serde_json::to_string_pretty(&self.store.conflict_list(n)?)?,
            Control::TxMap => {
                toml::to_string_pretty(&self.store.view_state(&View::Transaction(n.into()))?.0)?
            }
            Control::TxStatus => self.store.conn()?.query_row(
                "SELECT state FROM activations WHERE id=?1",
                [n],
                |r| r.get(0),
            )?,
        };
                Ok(s.into_bytes())
            })?;
        Ok(result.as_ref().clone())
    }
    fn stat(&self, p: &str) -> Result<Entry> {
        if let Some(b) = self.temps.get(p) {
            return Ok(Entry {
                kind: Kind::File,
                mode: 0o600,
                content: String::new(),
                size: b.len() as u64,
            });
        }
        match self.node(p)? {
            Node::Dir => Ok(directory()),
            Node::Control(n, c) => Ok(Entry {
                kind: Kind::File,
                mode: if matches!(c, Control::Draft) {
                    0o644
                } else {
                    0o444
                },
                content: crate::cache::hash(&self.control(&n, &c)?),
                size: self.control(&n, &c)?.len() as u64,
            }),
            Node::Source(v, p) => {
                let mut e = self.store.entry(&v, &p)?;
                if e.kind != Kind::Directory {
                    e.size = self.store.traversal_size(&v, &p, &e)?;
                }
                Ok(e)
            }
        }
    }
    fn attr(&self, ino: u64, e: &Entry) -> FileAttr {
        let p = self.path(ino).unwrap_or_default();
        let time = match self.node(&p) {
            Ok(Node::Source(v, path)) => self
                .store
                .view_snapshot(&v)
                .map(|s| s.time(&path))
                .unwrap_or(UNIX_EPOCH),
            _ => {
                UNIX_EPOCH + Duration::from_nanos(self.store.runtime.stamp.load(Ordering::Acquire))
            }
        };
        self.inodes
            .lock()
            .unwrap()
            .saved
            .insert(ino, (e.clone(), time));
        make_attr(ino, e, time)
    }
    fn children(&self, p: &str) -> Result<Vec<String>> {
        let parts: Vec<_> = p.split('/').filter(|x| !x.is_empty()).collect();
        let mut result = match parts.as_slice() {
            [] => vec![
                "workspaces".into(),
                "snapshots".into(),
                "transactions".into(),
            ],
            ["workspaces"] | ["snapshots"] => self.store.names()?,
            ["workspaces", _] => vec![
                "VERSION_MAP".into(),
                "ACTIVE_VERSION_MAP".into(),
                "STATUS".into(),
                "READONLY".into(),
                "depot".into(),
            ],
            ["snapshots", n] => self
                .store
                .snapshots(n)?
                .iter()
                .map(|i| i.to_string())
                .collect(),
            ["snapshots", _, _] => vec![
                "VERSION_MAP".into(),
                "ACTIVE_VERSION_MAP".into(),
                "READONLY".into(),
                "depot".into(),
            ],
            ["transactions"] => {
                let c = self.store.conn()?;
                let mut q = c.prepare("SELECT id FROM activations ORDER BY id")?;
                let r = q
                    .query_map([], |r| r.get(0))?
                    .collect::<std::result::Result<Vec<String>, _>>()?;
                r
            }
            ["transactions", _] => vec![
                "STATUS".into(),
                "PROPOSED_VERSION_MAP".into(),
                "conflicts.json".into(),
                "depot".into(),
            ],
            _ => {
                if let Node::Source(v, path) = self.node(p)? {
                    self.store.children(&v, &path)?.into_keys().collect()
                } else {
                    return Err(error("not a directory"));
                }
            }
        };
        for temp in self.temps.keys() {
            if Path::new(temp)
                .parent()
                .is_some_and(|parent| parent == Path::new(p))
            {
                result.push(
                    Path::new(temp)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        result.sort();
        result.dedup();
        Ok(result)
    }
    fn directory_handle(&self, path: &str) -> Result<DirectoryHandle> {
        let epoch = self.store.runtime.epoch.load(Ordering::Acquire);
        let key = if let Node::Source(view, relative) = self.node(path)? {
            let state = self.store.view_snapshot(&view)?;
            self.store.note_listing(&view, &state, &relative)?;
            format!(
                "listing:{view:?}:{}:{}:{path}",
                state.lineage,
                state.times.get(&relative).unwrap_or(&state.stamp)
            )
        } else {
            format!("mutable:dirhandle:{epoch}:{path}")
        };
        let entries = if let Some(entries) = self.store.runtime.get::<Vec<DirectoryEntry>>(&key) {
            entries
        } else {
            let mut entries = vec![(".".into(), directory()), ("..".into(), directory())];
            if let Node::Source(view, relative) = self.node(path)? {
                let state = self.store.view_snapshot(&view)?;
                entries.extend(
                    self.store
                        .directory_metadata(&state, &relative)?
                        .iter()
                        .map(|(n, e)| (n.clone(), e.clone())),
                );
            } else {
                for name in self.children(path)? {
                    let full = crate::model::join(path, &name);
                    let entry = match self.node(&full)? {
                        Node::Source(v, p) => self.store.entry(&v, &p)?,
                        _ => self.stat(&full)?,
                    };
                    entries.push((name, entry));
                }
            }
            let entries = entries
                .into_iter()
                .map(|(name, entry)| -> Result<DirectoryEntry> {
                    let full = directory_path(path, &name);
                    let (time, generation) = match self.node(&full)? {
                        Node::Source(view, relative) => {
                            let state = self.store.view_snapshot(&view)?;
                            (state.time(&relative), state.generation())
                        }
                        _ => (
                            UNIX_EPOCH
                                + Duration::from_nanos(
                                    self.store.runtime.stamp.load(Ordering::Acquire),
                                ),
                            String::new(),
                        ),
                    };
                    Ok(DirectoryEntry {
                        name,
                        entry,
                        time,
                        generation,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let bytes = entries
                .iter()
                .map(|r| r.name.len() + r.entry.content.len() + r.generation.len() + 192)
                .sum();
            self.store.runtime.put(key, entries, bytes)?
        };
        Ok(DirectoryHandle {
            path: path.into(),
            epoch,
            entries,
        })
    }
    fn directory_page(&self, fh: u64, offset: i64) -> Result<DirectoryHandle> {
        let handle = self
            .directories
            .lock()
            .unwrap()
            .get(&fh)
            .cloned()
            .ok_or_else(|| error("invalid directory handle"))?;
        if offset < 0 || offset as usize > handle.entries.len() {
            return Err(error("invalid directory offset"));
        }
        if offset == 0 && handle.epoch != self.store.runtime.epoch.load(Ordering::Acquire) {
            let new = self.directory_handle(&handle.path)?;
            self.directories.lock().unwrap().insert(fh, new.clone());
            return Ok(new);
        }
        Ok(handle)
    }
    fn reply_directory(&self, handle: DirectoryHandle, offset: i64, mut reply: ReplyDirectory) {
        for (index, record) in handle.entries.iter().enumerate().skip(offset as usize) {
            let path = directory_path(&handle.path, &record.name);
            let ino = self
                .inodes
                .lock()
                .unwrap()
                .ids
                .get(&path)
                .copied()
                .unwrap_or(0);
            if reply.add(
                ino,
                (index + 1) as i64,
                file_type(&record.entry.kind),
                &record.name,
            ) {
                break;
            }
        }
        reply.ok();
    }
    fn reply_directory_plus(
        &self,
        handle: DirectoryHandle,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let fresh = handle.epoch == self.store.runtime.epoch.load(Ordering::Acquire);
        for (index, record) in handle.entries.iter().enumerate().skip(offset as usize) {
            let path = directory_path(&handle.path, &record.name);
            let size = if fresh {
                metadata::cached_only(|| self.store.known_size(&record.entry))
                    .ok()
                    .flatten()
            } else {
                None
            };
            if let Some(size) = size {
                let mut entry = record.entry.clone();
                entry.size = size;
                let ino = self.inode_key(&path, &record.generation);
                let attr = make_attr(ino, &entry, record.time);
                if reply.add(
                    ino,
                    (index + 1) as i64,
                    &record.name,
                    &self.ttl(&path),
                    &attr,
                    0,
                ) {
                    break;
                }
                self.inodes
                    .lock()
                    .unwrap()
                    .saved
                    .insert(ino, (entry, record.time));
                if record.name != "." && record.name != ".." {
                    self.reference(ino);
                }
            } else {
                let attr = make_attr(0, &record.entry, record.time);
                if reply.add(
                    0,
                    (index + 1) as i64,
                    &record.name,
                    &Duration::ZERO,
                    &attr,
                    0,
                ) {
                    break;
                }
            }
        }
        reply.ok();
    }
    fn generation(&self, node: &Node) -> Result<Option<String>> {
        match node {
            Node::Source(v, _) => Ok(Some(self.store.view_snapshot(v)?.generation())),
            _ => Ok(None),
        }
    }
    fn handle_bytes(&self, h: &Handle) -> Result<Vec<u8>> {
        if let Some(b) = &h.bytes {
            return Ok(b.clone());
        }
        if self.generation(&h.node)? != h.generation {
            return self.store.bytes(&h.entry);
        }
        self.read_node(&h.node)
            .or_else(|_| self.store.bytes(&h.entry))
    }
    fn read_node(&self, node: &Node) -> Result<Vec<u8>> {
        match node {
            Node::Control(n, c) => self.control(n, c),
            Node::Source(v, p) => self.store.read(v, p),
            Node::Dir => Err(error("is a directory")),
        }
    }
    fn write_node(&self, node: &Node, b: &[u8], entry: &Entry) -> Result<()> {
        match node {
            Node::Control(n, Control::Draft) => self.store.set_draft(n, std::str::from_utf8(b)?),
            Node::Source(v, p) => self
                .store
                .write_file(v, p, b, entry.mode, entry.kind.clone()),
            _ => Err(error("read-only view")),
        }
    }
    fn child_path(&self, parent: u64, name: &OsStr) -> Result<String> {
        let n = name
            .to_str()
            .ok_or_else(|| error("non-UTF8 path unsupported"))?;
        if n.contains('/') || n == "." || n == ".." {
            return Err(error("invalid path"));
        }
        Ok(crate::model::join(&self.path(parent)?, n))
    }
}
fn file_type(kind: &Kind) -> FileType {
    match kind {
        Kind::File => FileType::RegularFile,
        Kind::Directory => FileType::Directory,
        Kind::Symlink => FileType::Symlink,
    }
}
fn directory_path(parent: &str, name: &str) -> String {
    match name {
        "." => parent.into(),
        ".." => metadata::split(parent).0.into(),
        _ => crate::model::join(parent, name),
    }
}
fn errno(e: &(dyn std::error::Error + Send + Sync)) -> i32 {
    let s = e.to_string();
    if s.contains("Resource temporarily unavailable") {
        return libc::EAGAIN;
    }
    if s.contains("Cannot allocate memory") {
        return libc::ENOMEM;
    }
    if s.contains("not found") || s.contains("Query returned no rows") {
        libc::ENOENT
    } else if s.contains("read-only") {
        libc::EROFS
    } else if s.contains("frozen") || s.contains("boundary") {
        libc::EBUSY
    } else if s.contains("stale generation") {
        libc::ESTALE
    } else if s.contains("already exists") {
        libc::EEXIST
    } else if s.contains("is a directory") {
        libc::EISDIR
    } else if s.contains("invalid") {
        libc::EINVAL
    } else {
        eprintln!("gtl FUSE error: {e}");
        libc::EIO
    }
}
impl Filesystem for Fs {
    fn init(
        &mut self,
        _: &Request<'_>,
        config: &mut KernelConfig,
    ) -> std::result::Result<(), libc::c_int> {
        // Notifications are installed by mount() before Session::run. Their
        // coherence guarantee does not depend on optional READDIRPLUS_AUTO.
        let _ = config.add_capabilities(fuser::consts::FUSE_DO_READDIRPLUS);
        let _ = config.add_capabilities(fuser::consts::FUSE_READDIRPLUS_AUTO);
        self.ttl.store(1, Ordering::Release);
        Ok(())
    }
    fn forget(&mut self, _: &Request<'_>, ino: u64, nlookup: u64) {
        let mut table = self.inodes.lock().unwrap();
        let n = table.references.entry(ino).or_default();
        *n = n.saturating_sub(nlookup);
        table.collect(ino);
    }

    fn lookup(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        self.store
            .runtime
            .metrics
            .lookups
            .fetch_add(1, Ordering::Relaxed);
        let path = match self.child_path(parent, name) {
            Ok(p) => p,
            Err(e) => {
                reply.error(errno(&*e));
                return;
            }
        };
        let work = |reader: &Fs| -> Result<(u64, FileAttr)> {
            let e = reader.stat(&path)?;
            let ino = reader.inode(&path);
            Ok((ino, reader.attr(ino, &e)))
        };
        let runtime = self.store.runtime.clone();
        let fence = runtime.reply_fence.read().unwrap();
        match metadata::cached_only(|| work(self)) {
            Ok((ino, attr)) => {
                self.reference(ino);
                reply.entry(&self.ttl(&path), &attr, 0);
            }
            Err(e) if metadata::would_block(&*e) => {
                drop(fence);
                let reader = self.reader();
                self.enqueue(move |accepted| {
                    if !accepted {
                        reply.error(libc::EAGAIN);
                        return;
                    }
                    crate::workers::complete(
                        &reader.store.runtime,
                        || {
                            let e = reader.stat(&path)?;
                            let ino = reader.inode(&path);
                            Ok((ino, reader.attr(ino, &e)))
                        },
                        |result| match result {
                            Ok((ino, attr)) => {
                                reader.reference(ino);
                                reply.entry(&reader.ttl(&path), &attr, 0)
                            }
                            Err(e) => reply.error(errno(&*e)),
                        },
                    );
                });
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn getattr(&mut self, _: &Request<'_>, ino: u64, reply: ReplyAttr) {
        self.store
            .runtime
            .metrics
            .getattrs
            .fetch_add(1, Ordering::Relaxed);
        let path = match self.path(ino) {
            Ok(p) => p,
            Err(e) => {
                reply.error(errno(&*e));
                return;
            }
        };
        let runtime = self.store.runtime.clone();
        let fence = runtime.reply_fence.read().unwrap();
        let work_path = path.clone();
        let work = move |reader: &Fs| -> Result<FileAttr> {
            let path = &work_path;
            // Resolve before deciding whether this inode belongs to a retired generation.
            if let Some((entry, stamp)) = reader.inodes.lock().unwrap().detached.get(&ino).cloned()
            {
                return Ok(make_attr(ino, &entry, stamp));
            }
            if let Ok(Node::Source(view, _)) = reader.node(path) {
                reader.store.view_snapshot(&view)?;
            }
            reader.inode(path);
            if let Some((entry, stamp)) = reader.inodes.lock().unwrap().detached.get(&ino).cloned()
            {
                return Ok(make_attr(ino, &entry, stamp));
            }
            let e = reader.stat(path)?;
            Ok(reader.attr(ino, &e))
        };
        match metadata::cached_only(|| work(self)) {
            Ok(attr) => reply.attr(&self.ttl(&path), &attr),
            Err(e) if metadata::would_block(&*e) => {
                drop(fence);
                let reader = self.reader();
                self.enqueue(move |accepted| {
                    if !accepted {
                        reply.error(libc::EAGAIN);
                        return;
                    }
                    crate::workers::complete(
                        &reader.store.runtime,
                        || work(&reader),
                        |result| match result {
                            Ok(attr) => reply.attr(&reader.ttl(&path), &attr),
                            Err(e) => reply.error(errno(&*e)),
                        },
                    );
                });
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn opendir(&mut self, _: &Request<'_>, ino: u64, _: i32, reply: ReplyOpen) {
        let path = match self.path(ino) {
            Ok(p) => p,
            Err(e) => {
                reply.error(errno(&*e));
                return;
            }
        };
        let runtime = self.store.runtime.clone();
        let fence = runtime.reply_fence.read().unwrap();
        match metadata::cached_only(|| self.directory_handle(&path)) {
            Ok(handle) => {
                let fh = self.next_directory.fetch_add(1, Ordering::Relaxed);
                self.directories.lock().unwrap().insert(fh, handle);
                *self.inodes.lock().unwrap().opens.entry(ino).or_default() += 1;
                reply.opened(fh, if path.starts_with("snapshots/") { 8 } else { 0 });
            }
            Err(e) if metadata::would_block(&*e) => {
                drop(fence);
                let reader = self.reader();
                self.enqueue(move |accepted| {
                    if !accepted {
                        reply.error(libc::EAGAIN);
                        return;
                    }
                    crate::workers::complete(
                        &reader.store.runtime,
                        || reader.directory_handle(&path),
                        |result| match result {
                            Ok(handle) => {
                                let fh = reader.next_directory.fetch_add(1, Ordering::Relaxed);
                                reader.directories.lock().unwrap().insert(fh, handle);
                                *reader.inodes.lock().unwrap().opens.entry(ino).or_default() += 1;
                                reply.opened(fh, if path.starts_with("snapshots/") { 8 } else { 0 })
                            }
                            Err(e) => reply.error(errno(&*e)),
                        },
                    );
                });
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn releasedir(&mut self, _: &Request<'_>, ino: u64, fh: u64, _: i32, reply: ReplyEmpty) {
        self.directories.lock().unwrap().remove(&fh);
        let mut t = self.inodes.lock().unwrap();
        let n = t.opens.entry(ino).or_default();
        *n = n.saturating_sub(1);
        t.collect(ino);
        reply.ok();
    }
    fn readdir(&mut self, _: &Request<'_>, _: u64, fh: u64, offset: i64, reply: ReplyDirectory) {
        self.store
            .runtime
            .metrics
            .readdirs
            .fetch_add(1, Ordering::Relaxed);
        let runtime = self.store.runtime.clone();
        let fence = runtime.reply_fence.read().unwrap();
        match metadata::cached_only(|| self.directory_page(fh, offset)) {
            Ok(handle) => self.reply_directory(handle, offset, reply),
            Err(e) if metadata::would_block(&*e) => {
                drop(fence);
                let reader = self.reader();
                self.enqueue(move |ok| {
                    if !ok {
                        reply.error(libc::EAGAIN);
                        return;
                    }
                    crate::workers::complete(
                        &reader.store.runtime,
                        || reader.directory_page(fh, offset),
                        |result| match result {
                            Ok(h) => reader.reply_directory(h, offset, reply),
                            Err(e) => reply.error(errno(&*e)),
                        },
                    );
                });
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn readdirplus(
        &mut self,
        _: &Request<'_>,
        _: u64,
        fh: u64,
        offset: i64,
        reply: ReplyDirectoryPlus,
    ) {
        self.store
            .runtime
            .metrics
            .readdirplus
            .fetch_add(1, Ordering::Relaxed);
        let runtime = self.store.runtime.clone();
        let fence = runtime.reply_fence.read().unwrap();
        match metadata::cached_only(|| self.directory_page(fh, offset)) {
            Ok(handle) => self.reply_directory_plus(handle, offset, reply),
            Err(e) if metadata::would_block(&*e) => {
                drop(fence);
                let reader = self.reader();
                self.enqueue(move |ok| {
                    if !ok {
                        reply.error(libc::EAGAIN);
                        return;
                    }
                    crate::workers::complete(
                        &reader.store.runtime,
                        || reader.directory_page(fh, offset),
                        |result| match result {
                            Ok(h) => reader.reply_directory_plus(h, offset, reply),
                            Err(e) => reply.error(errno(&*e)),
                        },
                    );
                });
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn open(&mut self, _: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let result = (|| -> Result<Handle> {
            let p = self.path(ino)?;
            let e = self.stat(&p)?;
            if let Some(b) = self.temps.get(&p) {
                return Ok(Handle {
                    node: Node::Control(p, Control::Draft),
                    entry: e,
                    bytes: Some(b.clone()),
                    generation: None,
                });
            }
            let node = self.node(&p)?;
            if flags & libc::O_ACCMODE != libc::O_RDONLY {
                match &node {
                    Node::Source(View::Live(n), _) if !self.store.frozen(n)? => {}
                    Node::Source(View::Transaction(_), _) | Node::Control(_, Control::Draft) => {}
                    _ => return Err(error("read-only view")),
                }
            }
            Ok(Handle {
                generation: self.generation(&node)?,
                node,
                entry: e,
                bytes: None,
            })
        })();
        match result {
            Ok(h) => {
                let id = self.next_handle;
                self.next_handle += 1;
                self.handles.insert(id, h);
                self.file_inodes.insert(id, ino);
                *self.inodes.lock().unwrap().opens.entry(ino).or_default() += 1;
                reply.opened(id, 1)
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn read(
        &mut self,
        _: &Request<'_>,
        _: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _: i32,
        _: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let Some(handle) = self.handles.get(&fh).cloned() else {
            reply.error(libc::EBADF);
            return;
        };
        let reader = self.reader();
        // Payload fetches must not stall cached metadata on the dispatch thread.
        self.enqueue(move |accepted| {
            if !accepted {
                reply.error(libc::EAGAIN);
                return;
            }
            match reader.handle_bytes(&handle) {
                Ok(bytes) => {
                    let start = (offset as usize).min(bytes.len());
                    reply.data(&bytes[start..start.saturating_add(size as usize).min(bytes.len())]);
                }
                Err(e) => reply.error(errno(&*e)),
            }
        });
    }

    fn write(
        &mut self,
        _: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _: u32,
        _: i32,
        _: Option<u64>,
        reply: ReplyWrite,
    ) {
        let r = (|| -> Result<()> {
            let path = self.path(ino)?;
            if let Some(b) = self.temps.get_mut(&path) {
                b.resize(b.len().max(offset as usize + data.len()), 0);
                b[offset as usize..offset as usize + data.len()].copy_from_slice(data);
                if let Some(h) = self.handles.get_mut(&fh) {
                    h.bytes = Some(b.clone());
                }
                return Ok(());
            }
            let h = self
                .handles
                .get(&fh)
                .ok_or_else(|| error("invalid handle"))?
                .clone();
            if h.bytes.is_none() {
                if let Node::Source(v, p) = &h.node {
                    let entry = self.store.write_range(
                        v,
                        p,
                        h.generation
                            .as_deref()
                            .ok_or_else(|| error("invalid generation"))?,
                        offset as usize,
                        data,
                    )?;
                    for handle in self.handles.values_mut() {
                        if handle.node == h.node && handle.bytes.is_none() {
                            handle.entry = entry.clone();
                        }
                    }
                    return Ok(());
                }
            }
            if self.generation(&h.node)? != h.generation {
                return Err(error("stale generation: reopen after activation"));
            }
            let mut b = self.handle_bytes(&h)?;
            b.resize(b.len().max(offset as usize + data.len()), 0);
            b[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            if h.bytes.is_some() {
                if let Some(ino) = self.file_inodes.get(&fh).copied() {
                    for (other, handle) in &mut self.handles {
                        if self.file_inodes.get(other) == Some(&ino) {
                            handle.bytes = Some(b.clone());
                            handle.entry.size = b.len() as u64;
                        }
                    }
                    if let Some((entry, stamp)) = self.inodes.lock().unwrap().detached.get_mut(&ino)
                    {
                        entry.size = b.len() as u64;
                        *stamp = SystemTime::now();
                    }
                }
                return Ok(());
            }
            self.write_node(&h.node, &b, &h.entry)?;
            let entry = match &h.node {
                Node::Source(v, p) => Some(self.store.entry(v, p)?),
                _ => None,
            };
            for handle in self.handles.values_mut() {
                if handle.node == h.node && handle.bytes.is_none() {
                    if let Some(e) = &entry {
                        handle.entry = e.clone();
                    }
                }
            }
            Ok(())
        })();
        match r {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn release(
        &mut self,
        _: &Request<'_>,
        _: u64,
        fh: u64,
        _: i32,
        _: Option<u64>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        self.handles.remove(&fh);
        if let Some(ino) = self.file_inodes.remove(&fh) {
            let mut t = self.inodes.lock().unwrap();
            let n = t.opens.entry(ino).or_default();
            *n = n.saturating_sub(1);
            t.collect(ino);
        }
        reply.ok()
    }
    fn flush(&mut self, _: &Request<'_>, _: u64, _: u64, _: u64, reply: ReplyEmpty) {
        reply.ok()
    }
    fn fsync(&mut self, _: &Request<'_>, _: u64, _: u64, _: bool, reply: ReplyEmpty) {
        reply.ok()
    }
    fn create(
        &mut self,
        _: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _: u32,
        _: i32,
        reply: ReplyCreate,
    ) {
        let r = (|| -> Result<(String, Handle)> {
            let p = self.child_path(parent, name)?;
            if self.stat(&p).is_ok() {
                return Err(error("already exists"));
            }
            if p.split('/').count() == 3 && p.starts_with("workspaces/") {
                self.temps.insert(p.clone(), vec![]);
                return Ok((
                    p.clone(),
                    Handle {
                        node: Node::Control(p, Control::Draft),
                        entry: Entry {
                            kind: Kind::File,
                            mode: mode & 0o777,
                            content: self.store.cache.put(b"")?,
                            size: 0,
                        },
                        bytes: Some(vec![]),
                        generation: None,
                    },
                ));
            }
            let node = self.node(&p)?;
            let e = Entry {
                kind: Kind::File,
                mode: mode & 0o777,
                content: self.store.cache.put(b"")?,
                size: 0,
            };
            self.write_node(&node, b"", &e)?;
            Ok((
                p,
                Handle {
                    generation: self.generation(&node)?,
                    node,
                    entry: e,
                    bytes: None,
                },
            ))
        })();
        match r {
            Ok((p, h)) => {
                let ino = self.inode(&p);
                let attr = self.attr(ino, &h.entry);
                let fh = self.next_handle;
                self.next_handle += 1;
                self.handles.insert(fh, h);
                self.file_inodes.insert(fh, ino);
                *self.inodes.lock().unwrap().opens.entry(ino).or_default() += 1;
                self.reference(ino);
                reply.created(&Duration::ZERO, &attr, 0, fh, 1)
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn mkdir(
        &mut self,
        _: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _: u32,
        reply: ReplyEntry,
    ) {
        let r = (|| -> Result<(String, Entry)> {
            let p = self.child_path(parent, name)?;
            if self.stat(&p).is_ok() {
                return Err(error("already exists"));
            }
            let Node::Source(v, path) = self.node(&p)? else {
                return Err(error("read-only view"));
            };
            let mut e = directory();
            e.mode = mode & 0o777;
            self.store.mutate(&v, &path, Some(e.clone()))?;
            Ok((p, e))
        })();
        match r {
            Ok((p, e)) => {
                let i = self.inode(&p);
                self.reference(i);
                reply.entry(&Duration::ZERO, &self.attr(i, &e), 0)
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn unlink(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| -> Result<()> {
            let p = self.child_path(parent, name)?;
            if self.temps.remove(&p).is_some() {
                return Ok(());
            }
            let Node::Source(v, path) = self.node(&p)? else {
                return Err(error("read-only view"));
            };
            let mut e = self.store.entry(&v, &path)?;
            e.size = self.store.exact_size(&e)?;
            if e.kind == Kind::Directory {
                return Err(error("is a directory"));
            }
            for h in self.handles.values_mut() {
                if h.node == Node::Source(v.clone(), path.clone()) {
                    h.bytes = Some(self.store.bytes(&e)?);
                }
            }
            self.store.mutate(&v, &path, None)?;
            let mut table = self.inodes.lock().unwrap();
            if let Some(ino) = table.ids.remove(&p) {
                table.detached.insert(ino, (e, SystemTime::now()));
            }
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn rmdir(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let r = (|| -> Result<()> {
            let p = self.child_path(parent, name)?;
            if !self.children(&p)?.is_empty() {
                return Err(error("directory not empty"));
            }
            let Node::Source(v, path) = self.node(&p)? else {
                return Err(error("read-only view"));
            };
            self.store.mutate(&v, &path, None)
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(if e.to_string().contains("not empty") {
                libc::ENOTEMPTY
            } else {
                errno(&*e)
            }),
        }
    }
    fn rename(
        &mut self,
        _: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _: u32,
        reply: ReplyEmpty,
    ) {
        let r = (|| -> Result<()> {
            let from = self.child_path(parent, name)?;
            let to = self.child_path(newparent, newname)?;
            if from == to {
                return Ok(());
            }
            if let Some(bytes) = self.temps.get(&from).cloned() {
                if let Ok(Node::Control(n, Control::Draft)) = self.node(&to) {
                    self.store.set_draft(&n, std::str::from_utf8(&bytes)?)?;
                    self.temps.remove(&from);
                    return Ok(());
                }
                if self.node(&to).is_ok() || Path::new(&from).parent() != Path::new(&to).parent() {
                    return Err(error("protected mount boundary"));
                }
                self.temps.remove(&from);
                self.temps.insert(to, bytes);
                return Ok(());
            }
            let (Node::Source(v, a), Node::Source(v2, b)) = (self.node(&from)?, self.node(&to)?)
            else {
                return Err(error("protected mount boundary"));
            };
            if v != v2 {
                return Err(error("cross mount"));
            }
            let _guard = self.store.view_lock(&v)?;
            let (m, _) = self.store.view_state(&v)?;
            if m.owner(&a).map(|o| o.0.id.clone()) != m.owner(&b).map(|o| o.0.id.clone()) {
                return Err(error("cross mount"));
            }
            if m.boundary(&a) || m.boundary(&b) {
                return Err(error("protected mount boundary"));
            }
            let entries = self.store.entries(&v)?;
            let e = entries.get(&a).ok_or_else(|| error("not found"))?;
            if e.kind == Kind::Directory && b.starts_with(&(a.clone() + "/")) {
                return Err(error("invalid recursive rename"));
            }
            let mut changes = vec![];
            for (p, e) in &entries {
                if p == &a || p.starts_with(&(a.clone() + "/")) {
                    let dest = format!("{}{}", b, &p[a.len()..]);
                    changes.push((p.clone(), None));
                    changes.push((dest, Some(e.clone())));
                }
            }
            if let Some(overwritten) = entries.get(&b) {
                for h in self.handles.values_mut() {
                    if h.node == Node::Source(v.clone(), b.clone()) {
                        h.bytes = Some(self.store.bytes(overwritten)?);
                    }
                }
            }
            self.store.mutate_locked(&v, changes)?;
            {
                let mut table = self.inodes.lock().unwrap();
                if let Some(overwritten) = entries.get(&b) {
                    if let Some(ino) = table.ids.remove(&to) {
                        let saved = table
                            .saved
                            .get(&ino)
                            .cloned()
                            .unwrap_or((overwritten.clone(), SystemTime::now()));
                        table.detached.insert(ino, saved);
                    }
                }
                let moved: Vec<_> = table
                    .ids
                    .iter()
                    .filter(|(p, _)| *p == &from || p.starts_with(&(from.clone() + "/")))
                    .map(|(p, i)| (p.clone(), *i))
                    .collect();
                for (path, ino) in moved {
                    let new = format!("{}{}", to, &path[from.len()..]);
                    table.ids.remove(&path);
                    table.ids.insert(new.clone(), ino);
                    table.paths.insert(ino, new);
                }
            }
            for h in self.handles.values_mut() {
                if let Node::Source(hv, hp) = &mut h.node {
                    if *hv == v && (hp == &a || hp.starts_with(&(a.clone() + "/"))) {
                        *hp = format!("{}{}", b, &hp[a.len()..]);
                    }
                }
            }
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(if e.to_string().contains("cross mount") {
                libc::EXDEV
            } else {
                errno(&*e)
            }),
        }
    }
    fn setattr(
        &mut self,
        _: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _: Option<u32>,
        _: Option<u32>,
        size: Option<u64>,
        _: Option<TimeOrNow>,
        _: Option<TimeOrNow>,
        _: Option<SystemTime>,
        _: Option<u64>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<u32>,
        reply: ReplyAttr,
    ) {
        if size.is_some() || mode.is_some() {
            if let Some(h) = self
                .file_inodes
                .iter()
                .find(|(_, i)| **i == ino)
                .and_then(|(fh, _)| self.handles.get(fh))
            {
                if self.generation(&h.node).ok().flatten() != h.generation {
                    reply.error(libc::ESTALE);
                    return;
                }
            }
        }
        let detached = self.inodes.lock().unwrap().detached.get(&ino).cloned();
        if let Some((mut entry, _)) = detached {
            let result = (|| -> Result<()> {
                if let Some(mode) = mode {
                    entry.mode = mode & 0o777;
                }
                if let Some(size) = size {
                    let fh = self
                        .file_inodes
                        .iter()
                        .find(|(_, i)| **i == ino)
                        .map(|(fh, _)| *fh)
                        .ok_or_else(|| error("not found"))?;
                    let handle = self
                        .handles
                        .get(&fh)
                        .cloned()
                        .ok_or_else(|| error("not found"))?;
                    let mut bytes = self.handle_bytes(&handle)?;
                    bytes.resize(usize::try_from(size)?, 0);
                    for (fh, h) in &mut self.handles {
                        if self.file_inodes.get(fh) == Some(&ino) {
                            h.bytes = Some(bytes.clone());
                            h.entry.size = size;
                        }
                    }
                    entry.size = size;
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    let stamp = SystemTime::now();
                    self.inodes
                        .lock()
                        .unwrap()
                        .detached
                        .insert(ino, (entry.clone(), stamp));
                    reply.attr(&Duration::ZERO, &make_attr(ino, &entry, stamp));
                }
                Err(e) => reply.error(errno(&*e)),
            }
            return;
        }
        let r = (|| -> Result<Entry> {
            let p = self.path(ino)?;
            if let Some(b) = self.temps.get_mut(&p) {
                if let Some(size) = size {
                    b.resize(size as usize, 0);
                }
                return self.stat(&p);
            }
            let node = self.node(&p)?;
            let mut e = self.stat(&p)?;
            if let Some(mode) = mode {
                e.mode = mode & 0o777;
            }
            if e.kind == Kind::Directory {
                if let Node::Source(v, p) = node {
                    self.store.mutate(&v, &p, Some(e.clone()))?;
                } else {
                    return Err(error("read-only view"));
                }
            } else {
                let mut bytes = self.read_node(&node)?;
                if let Some(size) = size {
                    bytes.resize(size as usize, 0);
                }
                self.write_node(&node, &bytes, &e)?;
                e.size = bytes.len() as u64;
            }
            Ok(e)
        })();
        match r {
            Ok(e) => reply.attr(&Duration::ZERO, &self.attr(ino, &e)),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn symlink(
        &mut self,
        _: &Request<'_>,
        parent: u64,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let r = (|| -> Result<(String, Entry)> {
            let p = self.child_path(parent, name)?;
            let Node::Source(v, path) = self.node(&p)? else {
                return Err(error("read-only view"));
            };
            if self.store.entry(&v, &path).is_ok() {
                return Err(error("already exists"));
            }
            let b = target
                .to_str()
                .ok_or_else(|| error("non-UTF8 symlink"))?
                .as_bytes();
            self.store.write_file(&v, &path, b, 0o777, Kind::Symlink)?;
            Ok((p, self.store.entry(&v, &path)?))
        })();
        match r {
            Ok((p, e)) => {
                let i = self.inode(&p);
                self.reference(i);
                reply.entry(&Duration::ZERO, &self.attr(i, &e), 0)
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn readlink(&mut self, _: &Request<'_>, ino: u64, reply: ReplyData) {
        let r = self
            .path(ino)
            .and_then(|p| self.node(&p))
            .and_then(|n| self.read_node(&n));
        match r {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn getxattr(&mut self, _: &Request<'_>, ino: u64, name: &OsStr, size: u32, reply: ReplyXattr) {
        // ls and other tools probe ACL/security attributes on every entry.
        // These attributes are unsupported regardless of repository contents.
        if name != "user.ws.content_id" && name != "user.ws.digest.sha256" {
            reply.error(libc::ENODATA);
            return;
        }
        let r = (|| -> Result<Vec<u8>> {
            let p = self.path(ino)?;
            let Node::Source(v, path) = self.node(&p)? else {
                return Err(error("no attribute"));
            };
            let e = self.store.entry(&v, &path)?;
            if e.kind == Kind::Directory {
                return Err(error("no attribute"));
            }
            if name == "user.ws.content_id" {
                Ok(format!("v1:{}", e.content).into_bytes())
            } else if name == "user.ws.digest.sha256" {
                use sha2::{Digest, Sha256};
                Ok(Sha256::digest(self.store.bytes(&e)?).to_vec())
            } else {
                Err(error("no attribute"))
            }
        })();
        match r {
            Ok(b) => {
                if size == 0 {
                    reply.size(b.len() as u32)
                } else if size < (b.len() as u32) {
                    reply.error(libc::ERANGE)
                } else {
                    reply.data(&b)
                }
            }
            Err(e) => reply.error(if e.to_string() == "no attribute" {
                libc::ENODATA
            } else {
                errno(&*e)
            }),
        }
    }
}
fn mount_path_error(
    path: &Path,
    operation: &str,
    e: std::io::Error,
) -> Box<dyn std::error::Error + Send + Sync> {
    if e.raw_os_error() == Some(libc::ENOTCONN) {
        error(format!("disconnected FUSE mount at {}; detach the old mount with fusermount3 -uz PATH, then retry (PATH is this mount directory)", path.display()))
    } else {
        error(format!(
            "{operation} mount directory {}: {e}",
            path.display()
        ))
    }
}
pub fn mount(store: Store, path: &Path) -> Result<()> {
    // create_dir_all masks stat failures on stale FUSE endpoints as EEXIST.
    // Inspect first, preserving the actual error and an actionable recovery path.
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(error(format!(
                "mount path is not a directory: {}",
                path.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)
                .map_err(|e| error(format!("create mount directory {}: {e}", path.display())))?;
        }
        Err(e) => return Err(mount_path_error(path, "inspect", e)),
    }
    let path = std::fs::canonicalize(path).map_err(|e| mount_path_error(path, "resolve", e))?;
    if path.starts_with(&store.root) || store.root.starts_with(&path) {
        return Err(error("mount and state directories must not overlap"));
    }
    // Cached root attributes can outlive a dead daemon. Opening/enumerating the
    // directory must preserve ENOTCONN too, even when the initial stat succeeded.
    let mut entries = std::fs::read_dir(&path).map_err(|e| mount_path_error(&path, "read", e))?;
    if let Some(entry) = entries.next() {
        entry.map_err(|e| mount_path_error(&path, "read", e))?;
        return Err(error("mount directory must be empty"));
    }

    let auto = store.clone();
    let runtime = store.runtime.clone();
    let fs = Fs::new(store);
    let inodes = fs.inodes.clone();
    let mut session = fuser::Session::new(
        fs,
        &path,
        &[
            MountOption::FSName("gitalator".into()),
            MountOption::DefaultPermissions,
            MountOption::RW,
        ],
    )
    .map_err(|e| error(format!("attach FUSE mount {}: {e}", path.display())))?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        let mut ticks = 0;
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
            if let Err(e) = auto.auto_snapshots() {
                eprintln!("automatic snapshot failed: {e}");
            }
            if let Err(e) = auto.runtime.flush() {
                eprintln!("metadata publication failed: {e}");
            }
            ticks += 1;
            if ticks % 10 == 0 {
                if let Err(e) = auto
                    .setting("cache_bytes", 1073741824)
                    .and_then(|max| auto.gc(false, max as u64))
                {
                    eprintln!("cache maintenance failed: {e}");
                }
            }
        }
    });
    let notifier = session.notifier();
    let mut unmounter = session.unmount_callable();
    let (send, recv) =
        std::sync::mpsc::channel::<std::sync::mpsc::SyncSender<std::result::Result<(), String>>>();
    let notification_runtime = runtime.clone();
    let notification_thread = std::thread::spawn(move || {
        while let Ok(done) = recv.recv() {
            let changed = std::mem::take(&mut *notification_runtime.changed.lock().unwrap());
            let records: Vec<_> = {
                let table = inodes.lock().unwrap();
                table
                    .paths
                    .iter()
                    .filter(|(_, p)| {
                        changed.is_empty()
                            || changed.iter().any(|c| {
                                c.is_empty() || c == *p || (c.ends_with('/') && p.starts_with(c))
                            })
                    })
                    .map(|(ino, p)| {
                        (
                            *ino,
                            p.clone(),
                            table.ids.get(metadata::split(p).0).copied(),
                        )
                    })
                    .collect()
            };
            let result = (|| -> std::io::Result<()> {
                for (ino, path, parent) in records {
                    notifier.inval_inode(ino, -1, 0)?;
                    if !path.is_empty() {
                        if let Some(parent) = parent {
                            notifier.inval_entry(parent, OsStr::new(metadata::split(&path).1))?;
                        }
                    }
                    notification_runtime
                        .metrics
                        .invalidations
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            })()
            .map_err(|e| e.to_string());
            if result.is_err() {
                notification_runtime
                    .unhealthy
                    .store(true, Ordering::Release);
                let _ = unmounter.unmount();
            }
            let _ = done.send(result);
        }
    });
    *runtime.invalidator.lock().unwrap() = Some(Arc::new(move || {
        let (done, result) = std::sync::mpsc::sync_channel(1);
        send.send(done)
            .map_err(|_| error("notification worker stopped"))?;
        result
            .recv()
            .map_err(|_| error("notification worker stopped"))?
            .map_err(error)
    }));
    runtime.pending.store(false, Ordering::Release);
    let result = session.run();
    runtime.invalidator.lock().unwrap().take();
    let _ = notification_thread.join();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = thread.join();
    result.map_err(|e| error(format!("serve FUSE mount {}: {e}", path.display())))?;
    Ok(())
}

fn make_attr(ino: u64, e: &Entry, time: SystemTime) -> FileAttr {
    FileAttr {
        ino,
        size: e.size,
        blocks: e.size.div_ceil(512),
        atime: UNIX_EPOCH,
        mtime: time,
        ctime: time,
        crtime: UNIX_EPOCH,
        kind: match e.kind {
            Kind::Directory => FileType::Directory,
            Kind::File => FileType::RegularFile,
            Kind::Symlink => FileType::Symlink,
        },
        perm: e.mode as u16,
        nlink: 1,
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}
