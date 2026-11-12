use crate::{
    model::{join, under, Kind},
    overlay::{io, Entry},
    store::{Store, Workspace},
    Result,
};
use fuser::*;
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, PermissionsExt},
    },
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
const TTL: Duration = Duration::ZERO;
struct Handle {
    ino: u64,
    workspace: Option<Arc<Mutex<Workspace>>>,
    file: Option<File>,
    bytes: Option<Vec<u8>>,
    directory: Option<Vec<(u64, FileType, String)>>,
    writable: bool,
    generation: Option<String>,
}
#[derive(Default)]
struct Nodes {
    next: u64,
    paths: BTreeMap<u64, String>,
    ids: BTreeMap<String, u64>,
    next_handle: u64,
    handles: BTreeMap<u64, Handle>,
}
pub struct Core {
    pub store: Arc<Mutex<Store>>,
    nodes: Mutex<Nodes>,
    pub notifier: Mutex<Option<fuser::Notifier>>,
}
pub struct Fs {
    pub core: Arc<Core>,
    pool: Pool,
}
struct Pool {
    send: std::sync::mpsc::SyncSender<Box<dyn FnOnce() + Send>>,
}
impl Pool {
    fn new() -> Self {
        let (send, recv) = std::sync::mpsc::sync_channel::<Box<dyn FnOnce() + Send>>(128);
        let recv = Arc::new(Mutex::new(recv));
        for _ in 0..8 {
            let r = recv.clone();
            std::thread::spawn(move || loop {
                let job = r.lock().unwrap().recv();
                match job {
                    Ok(job) => job(),
                    Err(_) => break,
                }
            });
        }
        Self { send }
    }
    fn submit(&self, job: impl FnOnce() + Send + 'static) {
        if let Err(std::sync::mpsc::TrySendError::Full(job)) = self.send.try_send(Box::new(job)) {
            job();
        }
    }
}
pub fn errno(e: &(dyn std::error::Error + Send + Sync + 'static)) -> i32 {
    e.downcast_ref::<std::io::Error>()
        .and_then(|e| e.raw_os_error())
        .unwrap_or(libc::EIO)
}
fn filetype(k: &Kind) -> FileType {
    match k {
        Kind::File => FileType::RegularFile,
        Kind::Directory => FileType::Directory,
        Kind::Symlink => FileType::Symlink,
    }
}
fn attr(ino: u64, e: &Entry) -> FileAttr {
    FileAttr {
        ino,
        size: e.size,
        blocks: e.size.div_ceil(512),
        atime: e.time,
        mtime: e.time,
        ctime: e.time,
        crtime: SystemTime::UNIX_EPOCH,
        kind: filetype(&e.kind),
        perm: (e.mode & 0o777) as u16,
        nlink: if e.kind == Kind::Directory { 2 } else { 1 },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}
fn dir() -> Entry {
    Entry {
        kind: Kind::Directory,
        mode: 0o755,
        size: 0,
        time: SystemTime::UNIX_EPOCH,
        physical: None,
        bytes: None,
    }
}
impl Core {
    pub fn new(store: Arc<Mutex<Store>>) -> Arc<Self> {
        Arc::new(Self {
            store,
            nodes: Mutex::new(Nodes {
                next: 2,
                paths: BTreeMap::from([(1, String::new())]),
                ids: BTreeMap::from([(String::new(), 1)]),
                next_handle: 1,
                ..Default::default()
            }),
            notifier: Mutex::new(None),
        })
    }
    fn path(&self, ino: u64) -> Result<String> {
        self.nodes
            .lock()
            .unwrap()
            .paths
            .get(&ino)
            .cloned()
            .ok_or_else(|| io(libc::ESTALE))
    }
    fn inode(&self, p: &str) -> u64 {
        let mut n = self.nodes.lock().unwrap();
        if let Some(i) = n.ids.get(p) {
            return *i;
        }
        let i = n.next;
        n.next += 1;
        n.paths.insert(i, p.into());
        n.ids.insert(p.into(), i);
        i
    }
    fn child(&self, parent: u64, name: &OsStr) -> Result<String> {
        let name = name.to_str().ok_or_else(|| io(libc::EINVAL))?;
        if name.contains('/') || name == "." || name == ".." {
            return Err(io(libc::EINVAL));
        }
        Ok(join(&self.path(parent)?, name))
    }
    fn workspace(&self, p: &str) -> Result<(Arc<Mutex<Workspace>>, String)> {
        let (name, rel) = p.split_once('/').unwrap_or((p, ""));
        let w = self.store.lock().unwrap().get(name)?;
        Ok((w, rel.into()))
    }
    fn entry(&self, p: &str) -> Result<Entry> {
        if p.is_empty() {
            return Ok(dir());
        }
        let (w, rel) = self.workspace(p)?;
        let e = w.lock().unwrap().entry(&rel)?;
        Ok(e)
    }
    fn stat(&self, ino: u64) -> Result<FileAttr> {
        if let Ok(p) = self.path(ino) {
            return Ok(attr(ino, &self.entry(&p)?));
        }
        let n = self.nodes.lock().unwrap();
        let h = n
            .handles
            .values()
            .find(|h| h.ino == ino)
            .ok_or_else(|| io(libc::ESTALE))?;
        let m = h
            .file
            .as_ref()
            .ok_or_else(|| io(libc::ESTALE))?
            .metadata()?;
        let mut e = dir();
        e.kind = Kind::File;
        e.mode = m.permissions().mode();
        e.size = m.len();
        e.time = m.modified()?;
        Ok(attr(ino, &e))
    }
    fn add_handle(&self, mut h: Handle) -> Result<u64> {
        if let Some(w) = &h.workspace {
            let mut w = w.lock().unwrap();
            w.check()?;
            if h.generation.as_ref() != Some(&w.record.generation) {
                return Err(io(libc::ESTALE));
            }
            w.handles += 1;
        }
        let mut n = self.nodes.lock().unwrap();
        let id = n.next_handle;
        n.next_handle += 1;
        // A copy-up preserves inode identity for every already-open descriptor.
        if h.writable {
            if let Some(f) = h.file.as_mut() {
                for old in n
                    .handles
                    .values_mut()
                    .filter(|x| x.ino == h.ino && x.file.is_some())
                {
                    old.file = Some(f.try_clone().unwrap());
                }
            }
        }
        n.handles.insert(id, h);
        Ok(id)
    }
    fn open(&self, ino: u64, flags: i32) -> Result<u64> {
        let p = self.path(ino)?;
        let (w, rel) = self.workspace(&p)?;
        let guard = w.lock().unwrap();
        let e = guard.entry(&rel)?;
        let generation = Some(guard.record.generation.clone());
        if e.kind != Kind::File {
            return Err(io(libc::EISDIR));
        }
        let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
        if e.bytes.is_some() {
            if writable {
                return Err(io(libc::EROFS));
            }
            drop(guard);
            return self.add_handle(Handle {
                ino,
                workspace: Some(w),
                file: None,
                bytes: e.bytes,
                directory: None,
                writable: false,
                generation,
            });
        }
        let f = guard.open_file(&rel, flags)?;
        drop(guard);
        self.add_handle(Handle {
            ino,
            workspace: Some(w),
            file: Some(f),
            bytes: None,
            directory: None,
            writable,
            generation,
        })
    }
    fn release(&self, fh: u64) {
        let h = self.nodes.lock().unwrap().handles.remove(&fh);
        if let Some(h) = h {
            if let Some(w) = h.workspace {
                w.lock().unwrap().handles -= 1;
            }
        }
    }
    fn unlink_nodes(&self, p: &str) {
        let mut n = self.nodes.lock().unwrap();
        let keys: Vec<_> = n.ids.keys().filter(|k| under(k, p)).cloned().collect();
        for k in keys {
            if let Some(i) = n.ids.remove(&k) {
                n.paths.remove(&i);
            }
        }
    }
    pub fn invalidate(&self, name: &str) -> Result<()> {
        let n = self.nodes.lock().unwrap();
        if let Some(notifier) = self.notifier.lock().unwrap().as_ref() {
            for (p, ino) in &n.ids {
                if under(p, name) {
                    let _ = notifier.inval_inode(*ino, 0, 0);
                    if let Some((parent, leaf)) = p.rsplit_once('/') {
                        if let Some(parent) = n.ids.get(parent) {
                            let _ = notifier.inval_entry(*parent, OsStr::new(leaf));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
impl Fs {
    pub fn new(core: Arc<Core>) -> Self {
        Self {
            core,
            pool: Pool::new(),
        }
    }
}
impl Filesystem for Fs {
    fn lookup(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let c = self.core.clone();
        let p = c.child(parent, name);
        self.pool.submit(move || {
            let r = p.and_then(|p| {
                let e = c.entry(&p)?;
                Ok(attr(c.inode(&p), &e))
            });
            match r {
                Ok(a) => reply.entry(&TTL, &a, 0),
                Err(e) => reply.error(errno(&*e)),
            }
        });
    }
    fn getattr(&mut self, _: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let c = self.core.clone();
        self.pool.submit(move || match c.stat(ino) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(errno(&*e)),
        });
    }
    fn readlink(&mut self, _: &Request<'_>, ino: u64, reply: ReplyData) {
        let r: Result<_> = (|| {
            let e = self.core.entry(&self.core.path(ino)?)?;
            if e.kind != Kind::Symlink {
                return Err(io(libc::EINVAL));
            }
            if let Some(b) = e.bytes {
                Ok(b)
            } else {
                Ok(std::fs::read_link(e.physical.unwrap())?
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec())
            }
        })();
        match r {
            Ok(b) => reply.data(&b),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn open(&mut self, _: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        match self.core.open(ino, flags) {
            Ok(h) => reply.opened(h, 0),
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
        let c = self.core.clone();
        self.pool.submit(move || {
            let r: Result<_> = (|| {
                if offset < 0 {
                    return Err(io(libc::EINVAL));
                }
                let n = c.nodes.lock().unwrap();
                let h = n.handles.get(&fh).ok_or_else(|| io(libc::EBADF))?;
                if let Some(bytes) = &h.bytes {
                    let start = (offset as usize).min(bytes.len());
                    return Ok(bytes[start..(start + size as usize).min(bytes.len())].to_vec());
                }
                let mut bytes = vec![0; size as usize];
                let len = h
                    .file
                    .as_ref()
                    .unwrap()
                    .read_at(&mut bytes, offset as u64)?;
                bytes.truncate(len);
                Ok(bytes)
            })();
            match r {
                Ok(b) => reply.data(&b),
                Err(e) => reply.error(errno(&*e)),
            }
        });
    }
    fn write(
        &mut self,
        _: &Request<'_>,
        _: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _: u32,
        _: i32,
        _: Option<u64>,
        reply: ReplyWrite,
    ) {
        let r: Result<_> = (|| {
            if offset < 0 {
                return Err(io(libc::EINVAL));
            }
            let n = self.core.nodes.lock().unwrap();
            let h = n.handles.get(&fh).ok_or_else(|| io(libc::EBADF))?;
            if !h.writable {
                return Err(io(libc::EBADF));
            }
            let f = h.file.as_ref().unwrap();
            f.write_all_at(data, offset as u64)?;
            Ok(())
        })();
        match r {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn flush(&mut self, _: &Request<'_>, _: u64, fh: u64, _: u64, reply: ReplyEmpty) {
        let r: Result<_> = (|| {
            let n = self.core.nodes.lock().unwrap();
            if let Some(f) = n.handles.get(&fh).and_then(|h| h.file.as_ref()) {
                f.sync_all()?;
            }
            Ok(())
        })();
        respond(reply, r)
    }
    fn fsync(&mut self, req: &Request<'_>, ino: u64, fh: u64, _: bool, reply: ReplyEmpty) {
        self.flush(req, ino, fh, 0, reply)
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
        self.core.release(fh);
        reply.ok();
    }
    fn opendir(&mut self, _: &Request<'_>, ino: u64, _: i32, reply: ReplyOpen) {
        let r: Result<_> = (|| {
            let p = self.core.path(ino)?;
            let (workspace, names, generation) = if p.is_empty() {
                (
                    None,
                    self.core
                        .store
                        .lock()
                        .unwrap()
                        .workspaces
                        .keys()
                        .cloned()
                        .collect(),
                    None,
                )
            } else {
                let (w, rel) = self.core.workspace(&p)?;
                let guard = w.lock().unwrap();
                let names = guard.children(&rel)?;
                let generation = Some(guard.record.generation.clone());
                drop(guard);
                (Some(w), names, generation)
            };
            let parent = Path::new(&p)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or("");
            let mut entries = vec![
                (ino, FileType::Directory, ".".into()),
                (self.core.inode(parent), FileType::Directory, "..".into()),
            ];
            for name in names {
                let child = join(&p, &name);
                let kind = if workspace.is_none() {
                    Kind::Directory
                } else {
                    let (w, rel) = self.core.workspace(&child)?;
                    let e = w.lock().unwrap().metadata(&rel)?;
                    if let Some(e) = e {
                        e.kind
                    } else {
                        let w = w.lock().unwrap();
                        if let Ok(m) = std::fs::symlink_metadata(w.upper().join(&rel)) {
                            if m.is_dir() {
                                Kind::Directory
                            } else if m.file_type().is_symlink() {
                                Kind::Symlink
                            } else {
                                Kind::File
                            }
                        } else {
                            w.base
                                .get(&rel)
                                .map(|b| b.kind.clone())
                                .unwrap_or(Kind::Directory)
                        }
                    }
                };
                entries.push((self.core.inode(&child), filetype(&kind), name));
            }
            self.core.add_handle(Handle {
                ino,
                workspace,
                file: None,
                bytes: None,
                directory: Some(entries),
                writable: false,
                generation,
            })
        })();
        match r {
            Ok(h) => reply.opened(h, 0),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn readdir(
        &mut self,
        _: &Request<'_>,
        _: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let n = self.core.nodes.lock().unwrap();
        if let Some(entries) = n.handles.get(&fh).and_then(|h| h.directory.as_ref()) {
            for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset.max(0) as usize) {
                if reply.add(*ino, (i + 1) as i64, *kind, name) {
                    break;
                }
            }
            reply.ok();
        } else {
            reply.error(libc::EBADF);
        }
    }
    fn releasedir(&mut self, _: &Request<'_>, _: u64, fh: u64, _: i32, reply: ReplyEmpty) {
        self.core.release(fh);
        reply.ok();
    }
    fn create(
        &mut self,
        _: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _: i32,
        reply: ReplyCreate,
    ) {
        let r: Result<_> = (|| {
            let p = self.core.child(parent, name)?;
            let (w, rel) = self.core.workspace(&p)?;
            let guard = w.lock().unwrap();
            let file = guard.create_file(&rel, mode & !umask)?;
            let e = guard.entry(&rel)?;
            let generation = Some(guard.record.generation.clone());
            drop(guard);
            let ino = self.core.inode(&p);
            let fh = self.core.add_handle(Handle {
                ino,
                workspace: Some(w),
                file: Some(file),
                bytes: None,
                directory: None,
                writable: true,
                generation,
            })?;
            Ok((attr(ino, &e), fh))
        })();
        match r {
            Ok((a, h)) => reply.created(&TTL, &a, 0, h, 0),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn mkdir(
        &mut self,
        _: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let r: Result<_> = (|| {
            let p = self.core.child(parent, name)?;
            let (w, rel) = self.core.workspace(&p)?;
            let w = w.lock().unwrap();
            w.mkdir(&rel, mode & !umask)?;
            Ok(attr(self.core.inode(&p), &w.entry(&rel)?))
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, 0),
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
        let r: Result<_> = (|| {
            let p = self.core.child(parent, name)?;
            let (w, rel) = self.core.workspace(&p)?;
            let w = w.lock().unwrap();
            w.symlink(&rel, target)?;
            Ok(attr(self.core.inode(&p), &w.entry(&rel)?))
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, 0),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn unlink(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        respond(reply, self.remove(parent, name, false));
    }
    fn rmdir(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        respond(reply, self.remove(parent, name, true));
    }
    fn rename(
        &mut self,
        _: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let r: Result<_> = (|| {
            if flags != 0 {
                return Err(io(libc::EINVAL));
            }
            let src = self.core.child(parent, name)?;
            let dst = self.core.child(newparent, newname)?;
            let (a, rel) = self.core.workspace(&src)?;
            let (b, newrel) = self.core.workspace(&dst)?;
            if !Arc::ptr_eq(&a, &b) {
                return Err(io(libc::EXDEV));
            }
            a.lock().unwrap().rename(&rel, &newrel)?;
            self.core.unlink_nodes(&dst);
            let mut n = self.core.nodes.lock().unwrap();
            let old: Vec<_> = n
                .ids
                .iter()
                .filter(|(p, _)| under(p, &src))
                .map(|(p, i)| (p.clone(), *i))
                .collect();
            for (p, i) in old {
                n.ids.remove(&p);
                let next = format!("{}{}", dst, &p[src.len()..]);
                n.paths.insert(i, next.clone());
                n.ids.insert(next, i);
            }
            Ok(())
        })();
        respond(reply, r)
    }
    fn setattr(
        &mut self,
        _: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _: Option<SystemTime>,
        fh: Option<u64>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<u32>,
        reply: ReplyAttr,
    ) {
        let r: Result<_> = (|| {
            if uid.is_some_and(|u| u != unsafe { libc::getuid() })
                || gid.is_some_and(|g| g != unsafe { libc::getgid() })
            {
                return Err(io(libc::EPERM));
            }
            let f = if let Ok(path) = self.core.path(ino) {
                let (w, rel) = self.core.workspace(&path)?;
                let w = w.lock().unwrap();
                if w.entry(&rel)?.kind == Kind::Symlink {
                    return Err(io(libc::EOPNOTSUPP));
                }
                let dst = w.copy_up(&rel, false)?;
                let mut n = self.core.nodes.lock().unwrap();
                if let Some(fh) = fh {
                    if size.is_some() && !n.handles.get(&fh).is_some_and(|h| h.writable) {
                        return Err(io(libc::EBADF));
                    }
                }
                for h in n
                    .handles
                    .values_mut()
                    .filter(|h| h.ino == ino && h.file.is_some())
                {
                    h.file = Some(OpenOptions::new().read(true).write(h.writable).open(&dst)?);
                }
                OpenOptions::new()
                    .read(true)
                    .write(size.is_some())
                    .open(dst)?
            } else {
                let n = self.core.nodes.lock().unwrap();
                let h = n
                    .handles
                    .get(&fh.ok_or_else(|| io(libc::ESTALE))?)
                    .ok_or_else(|| io(libc::EBADF))?;
                if !h.writable {
                    return Err(io(libc::EBADF));
                }
                h.file
                    .as_ref()
                    .ok_or_else(|| io(libc::EBADF))?
                    .try_clone()?
            };
            if let Some(size) = size {
                f.set_len(size)?;
            }
            if let Some(mode) = mode {
                f.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))?;
            }
            if atime.is_some() || mtime.is_some() {
                let time = |t: TimeOrNow| match t {
                    TimeOrNow::SpecificTime(t) => t,
                    TimeOrNow::Now => SystemTime::now(),
                };
                let mut times = std::fs::FileTimes::new();
                if let Some(t) = atime {
                    times = times.set_accessed(time(t));
                }
                if let Some(t) = mtime {
                    times = times.set_modified(time(t));
                }
                f.set_times(times)?;
            }
            self.core.stat(ino)
        })();
        match r {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(errno(&*e)),
        }
    }
    fn statfs(&mut self, _: &Request<'_>, _: u64, reply: ReplyStatfs) {
        let root = self.core.store.lock().unwrap().root.clone();
        let r: Result<_> = (|| {
            let f = File::open(root)?;
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstatvfs(f.as_raw_fd(), &mut stat) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(stat)
        })();
        match r {
            Ok(s) => reply.statfs(
                s.f_blocks,
                s.f_bfree,
                s.f_bavail,
                s.f_files,
                s.f_ffree,
                s.f_bsize as u32,
                s.f_namemax as u32,
                s.f_frsize as u32,
            ),
            Err(_) => reply.error(libc::EIO),
        }
    }
}
impl Fs {
    fn remove(&self, parent: u64, name: &OsStr, directory: bool) -> Result<()> {
        let p = self.core.child(parent, name)?;
        let (w, rel) = self.core.workspace(&p)?;
        w.lock().unwrap().unlink(&rel, directory)?;
        self.core.unlink_nodes(&p);
        Ok(())
    }
}
fn respond(reply: ReplyEmpty, result: Result<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(errno(&*e)),
    }
}
