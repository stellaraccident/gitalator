use crate::{
    db::Store,
    error,
    model::{Entry, Kind},
    ops::{directory, View},
    Result,
};
use fuser::*;
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[derive(Clone, PartialEq, Eq)]
enum Node {
    Dir,
    Control(String, Control),
    Source(View, String),
}
#[derive(Clone, PartialEq, Eq)]
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
pub struct Fs {
    store: Store,
    paths: BTreeMap<u64, String>,
    ids: BTreeMap<String, u64>,
    next: u64,
    handles: BTreeMap<u64, Handle>,
    next_handle: u64,
    temps: BTreeMap<String, Vec<u8>>,
}
impl Fs {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            paths: BTreeMap::from([(1, String::new())]),
            ids: BTreeMap::from([(String::new(), 1)]),
            next: 2,
            handles: BTreeMap::new(),
            next_handle: 1,
            temps: BTreeMap::new(),
        }
    }
    fn inode(&mut self, p: &str) -> u64 {
        if let Some(i) = self.ids.get(p) {
            return *i;
        }
        let i = self.next;
        self.next += 1;
        self.paths.insert(i, p.into());
        self.ids.insert(p.into(), i);
        i
    }
    fn path(&self, ino: u64) -> Result<String> {
        self.paths
            .get(&ino)
            .cloned()
            .ok_or_else(|| error("not found"))
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
                content: String::new(),
                size: self.control(&n, &c)?.len() as u64,
            }),
            Node::Source(v, p) => {
                let mut e = self.store.entry(&v, &p)?;
                if e.kind != Kind::Directory {
                    e.size = self.store.bytes(&e)?.len() as u64;
                }
                Ok(e)
            }
        }
    }
    fn attr(&self, ino: u64, e: &Entry) -> FileAttr {
        let fingerprint = format!("{:?}:{}:{}:{}", e.kind, e.mode, e.content, e.size);
        let p = self.paths.get(&ino).cloned().unwrap_or_default();
        let time = self
            .store
            .attribute_time(&p, &fingerprint)
            .unwrap_or(SystemTime::UNIX_EPOCH);
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
                    let prefix = if path.is_empty() {
                        String::new()
                    } else {
                        path + "/"
                    };
                    self.store
                        .entries(&v)?
                        .keys()
                        .filter_map(|k| k.strip_prefix(&prefix))
                        .filter(|r| !r.is_empty() && !r.contains('/'))
                        .map(str::to_string)
                        .collect()
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
    fn generation(&self, node: &Node) -> Result<Option<String>> {
        match node {
            Node::Source(v, _) => Ok(Some(crate::cache::hash(
                serde_json::to_string(&self.store.view_state(v)?.0)?.as_bytes(),
            ))),
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
fn errno(e: &(dyn std::error::Error + Send + Sync)) -> i32 {
    let s = e.to_string();
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
    fn lookup(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self
            .child_path(parent, name)
            .and_then(|p| self.stat(&p).map(|e| (p, e)))
        {
            Ok((p, e)) => {
                let i = self.inode(&p);
                reply.entry(&Duration::ZERO, &self.attr(i, &e), 0)
            }
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn getattr(&mut self, _: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.path(ino).and_then(|p| self.stat(&p)) {
            Ok(e) => reply.attr(&Duration::ZERO, &self.attr(ino, &e)),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn readdir(
        &mut self,
        _: &Request<'_>,
        ino: u64,
        _: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let r = self
            .path(ino)
            .and_then(|p| self.children(&p).map(|c| (p, c)));
        match r {
            Ok((p, c)) => {
                let mut entries = vec![
                    (ino, FileType::Directory, ".".into()),
                    (1, FileType::Directory, "..".into()),
                ];
                for name in c {
                    let full = crate::model::join(&p, &name);
                    let metadata = match self.node(&full) {
                        Ok(Node::Source(v, p)) => self.store.entry(&v, &p),
                        _ => self.stat(&full),
                    };
                    if let Ok(e) = metadata {
                        let i = self.inode(&full);
                        entries.push((i, self.attr(i, &e).kind, name));
                    }
                }
                for (index, (i, kind, name)) in
                    entries.into_iter().enumerate().skip(offset as usize)
                {
                    if reply.add(i, (index + 1) as i64, kind, name) {
                        break;
                    }
                }
                reply.ok()
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
        let r = (|| -> Result<Vec<u8>> {
            let h = self
                .handles
                .get(&fh)
                .ok_or_else(|| error("invalid handle"))?;
            self.handle_bytes(h)
        })();
        match r {
            Ok(b) => {
                let start = (offset as usize).min(b.len());
                reply.data(&b[start..(start + size as usize).min(b.len())])
            }
            Err(e) => reply.error(errno(&*e)),
        }
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
                        if handle.node == h.node {
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
                if let Some(handle) = self.handles.get_mut(&fh) {
                    handle.bytes = Some(b);
                }
                return Ok(());
            }
            self.write_node(&h.node, &b, &h.entry)?;
            let entry = match &h.node {
                Node::Source(v, p) => Some(self.store.entry(v, p)?),
                _ => None,
            };
            for handle in self.handles.values_mut() {
                if handle.node == h.node {
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
            let e = self.store.entry(&v, &path)?;
            if e.kind == Kind::Directory {
                return Err(error("is a directory"));
            }
            for h in self.handles.values_mut() {
                if h.node == Node::Source(v.clone(), path.clone()) {
                    h.bytes = Some(self.store.bytes(&e)?);
                }
            }
            self.store.mutate(&v, &path, None)
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
            self.store.mutate_locked(&v, changes)?;
            for h in self.handles.values_mut() {
                if let Node::Source(_, hp) = &mut h.node {
                    if hp == &a || hp.starts_with(&(a.clone() + "/")) {
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
pub fn mount(store: Store, path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let path = std::fs::canonicalize(path)?;
    if path.starts_with(&store.root) || store.root.starts_with(&path) {
        return Err(error("mount and state directories must not overlap"));
    }
    if std::fs::read_dir(&path)?.next().is_some() {
        return Err(error("mount directory must be empty"));
    }
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let auto = store.clone();
    let thread = std::thread::spawn(move || {
        let mut ticks = 0;
        while !flag.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
            if let Err(e) = auto.auto_snapshots() {
                eprintln!("automatic snapshot failed: {e}");
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
    let result = fuser::mount2(
        Fs::new(store),
        path,
        &[
            MountOption::FSName("gitalator".into()),
            MountOption::DefaultPermissions,
            MountOption::RW,
        ],
    );
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = thread.join();
    result?;
    Ok(())
}
