use crate::{
    error, git,
    model::*,
    store::{walk, Journal, Workspace},
    util::{atomic, Lock},
    Result,
};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    os::unix::fs::{symlink, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};
#[derive(Clone)]
pub struct Entry {
    pub kind: Kind,
    pub mode: u32,
    pub size: u64,
    pub time: SystemTime,
    pub physical: Option<PathBuf>,
    pub bytes: Option<Vec<u8>>,
}
pub fn io(code: i32) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::from_raw_os_error(code).into()
}
impl Workspace {
    pub fn state_root(&self) -> &Path {
        self.root.parent().unwrap().parent().unwrap()
    }
    pub fn marker(&self, p: &str) -> Result<Option<Tombstone>> {
        let f = match self.safe(&self.tombs(), p) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        if !fs::symlink_metadata(&f).is_ok_and(|m| m.is_file()) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(f)?)?))
    }
    pub fn hidden(&self, p: &str) -> Result<bool> {
        let mut prefix = String::new();
        for c in p.split('/') {
            prefix = join(&prefix, c);
            if self.marker(&prefix)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn metadata(&self, p: &str) -> Result<Option<Entry>> {
        let file = |b: Vec<u8>| {
            Some(Entry {
                kind: Kind::File,
                mode: 0o444,
                size: b.len() as u64,
                time: SystemTime::UNIX_EPOCH,
                physical: None,
                bytes: Some(b),
            })
        };
        let link = |path: PathBuf| {
            Some(Entry {
                kind: Kind::Symlink,
                mode: 0o777,
                size: path.as_os_str().len() as u64,
                time: SystemTime::UNIX_EPOCH,
                physical: None,
                bytes: Some(path.as_os_str().as_encoded_bytes().to_vec()),
            })
        };
        if p == ".depotgit" {
            return Ok(link(self.depot()));
        }
        if p == ".depotstate.json" {
            let v = serde_json::json!({"format":1,"workspace":self.record.name,"workspace_id":self.record.id,"generation":self.record.generation,"depot_worktree":self.depot(),"depot_repository":self.record.depot_repo,"depot_bare":self.state_root().join("repos").join(&self.record.depot_repo),"branch":self.record.branch,"active_commit":self.record.active_commit,"editing":self.record.editing,"map":self.record.map});
            return Ok(file(serde_json::to_vec_pretty(&v)?));
        }
        for m in &self.record.map.mounts {
            let id = crate::util::hash(self.record.map.repositories[&m.repository].url.as_bytes());
            if p == join(&m.path, ".mountgit") {
                return Ok(link(self.state_root().join("repos").join(id)));
            }
            if p == join(&m.path, ".mountcommit") {
                return Ok(file(format!("{}\n", m.commit).into_bytes()));
            }
            if p == join(&m.path, ".mountstate.json") {
                return Ok(file(serde_json::to_vec_pretty(
                    &serde_json::json!({"format":1,"generation":self.record.generation,"mount_id":m.id,"repository":m.repository,"repository_id":id,"commit":m.commit,"subtree":m.subtree,"path":m.path,"object_format":if m.commit.len()==40{"sha1"}else{"sha256"}}),
                )?));
            }
        }
        Ok(None)
    }
    fn direct(&self, p: &str) -> Result<Entry> {
        if let Some(e) = self.metadata(p)? {
            return Ok(e);
        }
        if p.is_empty() {
            return Ok(Entry {
                kind: Kind::Directory,
                mode: 0o755,
                size: 0,
                time: SystemTime::UNIX_EPOCH,
                physical: None,
                bytes: None,
            });
        }
        let upper = self.safe(&self.upper(), p)?;
        if let Ok(m) = fs::symlink_metadata(&upper) {
            let kind = if m.is_dir() {
                Kind::Directory
            } else if m.file_type().is_symlink() {
                Kind::Symlink
            } else {
                Kind::File
            };
            return Ok(Entry {
                kind,
                mode: m.permissions().mode() & 0o777,
                size: m.len(),
                time: m.modified()?,
                physical: Some(upper),
                bytes: None,
            });
        }
        if self.hidden(p)? {
            return Err(io(libc::ENOENT));
        }
        let b = self.base.get(p).ok_or_else(|| io(libc::ENOENT))?;
        Ok(Entry {
            kind: b.kind.clone(),
            mode: if b.kind == Kind::Directory {
                0o755
            } else {
                b.mode
            },
            size: 0,
            time: SystemTime::UNIX_EPOCH,
            physical: None,
            bytes: None,
        })
    }
    pub fn entry(&self, p: &str) -> Result<Entry> {
        if !valid_path(p) {
            return Err(io(libc::EINVAL));
        }
        if self.metadata(p)?.is_none() {
            self.check()?;
        }
        let mut prefix = String::new();
        let parts: Vec<_> = p.split('/').collect();
        for c in parts.iter().take(parts.len().saturating_sub(1)) {
            prefix = join(&prefix, c);
            if self.direct(&prefix)?.kind != Kind::Directory {
                return Err(io(libc::ENOTDIR));
            }
        }
        let mut e = self.direct(p)?;
        if e.physical.is_none() && e.bytes.is_none() && e.kind != Kind::Directory {
            let path = self.base_file(p)?;
            e.size = fs::metadata(&path)?.len();
            if e.kind == Kind::Symlink {
                e.bytes = Some(fs::read(path)?);
            } else {
                e.physical = Some(path);
            }
        }
        Ok(e)
    }
    pub fn base_file(&self, p: &str) -> Result<PathBuf> {
        let b = self.base.get(p).ok_or_else(|| io(libc::ENOENT))?;
        let target = self.state_root().join("cache").join(&b.repo).join(&b.oid);
        if target.exists() {
            return Ok(target);
        }
        let _l = Lock::take(&self.state_root().join("locks").join(&b.repo))?;
        if !target.exists() {
            let repo = self.state_root().join("repos").join(&b.repo);
            let mut bytes = git::run(&repo, &["cat-file", "blob", &b.oid])?;
            if bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
                let mut cmd = std::process::Command::new("git");
                cmd.arg("-C")
                    .arg(&repo)
                    .args(["lfs", "smudge"])
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let result = git::execute_command(
                    &mut cmd,
                    Some(&bytes),
                    std::time::Duration::from_secs(300),
                )?;
                if !result.status.success() {
                    return Err(error("LFS hydration failed"));
                }
                bytes = result.stdout;
            }
            atomic(&target, &bytes)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o444))?;
        }
        Ok(target)
    }
    pub fn children(&self, p: &str) -> Result<Vec<String>> {
        self.check()?;
        if self.direct(p)?.kind != Kind::Directory {
            return Err(io(libc::ENOTDIR));
        }
        let mut names = BTreeSet::new();
        let prefix = if p.is_empty() {
            String::new()
        } else {
            format!("{p}/")
        };
        for key in self.base.keys() {
            if let Some(tail) = key.strip_prefix(&prefix) {
                if let Some(first) = tail.split('/').next() {
                    if !first.is_empty() {
                        names.insert(first.to_string());
                    }
                }
            }
        }
        let upper = self.safe(&self.upper(), p)?;
        if upper.is_dir() {
            for e in fs::read_dir(upper)? {
                names.insert(
                    e?.file_name()
                        .into_string()
                        .map_err(|_| error("non-UTF8 filename"))?,
                );
            }
        }
        if p.is_empty() {
            names.insert(".depotgit".into());
            names.insert(".depotstate.json".into());
        }
        if self.record.map.mounts.iter().any(|m| m.path == p) {
            for n in [".mountgit", ".mountcommit", ".mountstate.json"] {
                names.insert(n.into());
            }
        }
        Ok(names
            .into_iter()
            .filter(|n| self.direct(&join(p, n)).is_ok())
            .collect())
    }
    pub fn mutable(&self, p: &str) -> Result<()> {
        self.check()?;
        if !valid_path(p) || self.record.map.protected(p) {
            return Err(io(libc::EPERM));
        }
        Ok(())
    }
    fn parents(&self, p: &str) -> Result<()> {
        let parent = Path::new(p).parent().unwrap().to_str().unwrap();
        if self.entry(parent)?.kind != Kind::Directory {
            return Err(io(libc::ENOTDIR));
        }
        let upper = self.safe(&self.upper(), parent)?;
        fs::create_dir_all(upper)?;
        Ok(())
    }
    pub fn copy_up(&self, p: &str, truncate: bool) -> Result<PathBuf> {
        self.mutable(p)?;
        let e = self.entry(p)?;
        let dst = self.safe(&self.upper(), p)?;
        if fs::symlink_metadata(&dst).is_ok() {
            return Ok(dst);
        }
        self.parents(p)?;
        match e.kind {
            Kind::Directory => fs::create_dir(&dst)?,
            Kind::Symlink => {
                symlink(String::from_utf8(e.bytes.ok_or("missing symlink")?)?, &dst)?;
            }
            Kind::File => {
                let bytes = if truncate {
                    vec![]
                } else {
                    fs::read(e.physical.ok_or("missing base")?)?
                };
                atomic(&dst, &bytes)?;
                fs::set_permissions(&dst, fs::Permissions::from_mode(e.mode))?;
            }
        }
        Ok(dst)
    }
    pub fn open_file(&self, p: &str, flags: i32) -> Result<File> {
        let write = flags & libc::O_ACCMODE != libc::O_RDONLY;
        let e = self.entry(p)?;
        if e.kind != Kind::File {
            return Err(io(libc::EISDIR));
        }
        let path = if write {
            self.copy_up(p, flags & libc::O_TRUNC != 0)?
        } else {
            e.physical
                .ok_or("metadata file has no backing descriptor")?
        };
        Ok(OpenOptions::new()
            .read(true)
            .write(write)
            .truncate(write && flags & libc::O_TRUNC != 0)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?)
    }
    fn stage(&self, kind: Kind, mode: u32, target: Option<&Path>) -> Result<String> {
        let id = crate::store::token();
        let pending = self.root.join("pending");
        fs::create_dir_all(&pending)?;
        let path = pending.join(&id);
        match kind {
            Kind::Directory => {
                fs::create_dir(&path)?;
                fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o777))?;
                File::open(&path)?.sync_all()?;
            }
            Kind::Symlink => symlink(target.ok_or("missing symlink target")?, &path)?,
            Kind::File => {
                let f = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(mode & 0o777)
                    .open(&path)?;
                f.sync_all()?;
            }
        }
        File::open(pending)?.sync_all()?;
        Ok(id)
    }
    pub fn create_file(&self, p: &str, mode: u32) -> Result<File> {
        self.mutable(p)?;
        if self.direct(p).is_ok() {
            return Err(io(libc::EEXIST));
        }
        self.parents(p)?;
        let pending = self.stage(Kind::File, mode, None)?;
        self.transact(Journal {
            clear: vec![p.into()],
            install: Some((pending, p.into())),
            ..Default::default()
        })?;
        Ok(OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.safe(&self.upper(), p)?)?)
    }
    pub fn mkdir(&self, p: &str, mode: u32) -> Result<()> {
        self.mutable(p)?;
        if self.direct(p).is_ok() {
            return Err(io(libc::EEXIST));
        }
        self.parents(p)?;
        let mut mark = vec![];
        if let Some(mut t) = self.marker(p)? {
            t.kind = "opaque".into();
            mark.push((p.into(), t));
        }
        let pending = self.stage(Kind::Directory, mode, None)?;
        self.transact(Journal {
            clear: vec![p.into()],
            mark,
            install: Some((pending, p.into())),
            ..Default::default()
        })
    }
    pub fn symlink(&self, p: &str, target: &Path) -> Result<()> {
        self.mutable(p)?;
        if self.direct(p).is_ok() {
            return Err(io(libc::EEXIST));
        }
        self.parents(p)?;
        let pending = self.stage(Kind::Symlink, 0o777, Some(target))?;
        self.transact(Journal {
            clear: vec![p.into()],
            install: Some((pending, p.into())),
            ..Default::default()
        })
    }
    pub fn tombstone(&self, p: &str, opaque: bool) -> Result<Option<Tombstone>> {
        if self.hidden(p)? && self.marker(p)?.is_none() {
            return Ok(None);
        }
        Ok(self.base.get(p).and_then(|b| {
            self.record.map.owner(p).map(|m| Tombstone {
                format: 1,
                kind: if opaque { "opaque" } else { "delete" }.into(),
                mount_id: m.id.clone(),
                base_commit: m.commit.clone(),
                base_oid: b.oid.clone(),
                base_kind: b.kind.clone(),
            })
        }))
    }
    pub fn unlink(&self, p: &str, directory: bool) -> Result<()> {
        self.mutable(p)?;
        let e = self.entry(p)?;
        if directory != (e.kind == Kind::Directory) {
            return Err(io(if directory {
                libc::ENOTDIR
            } else {
                libc::EISDIR
            }));
        }
        if directory && !self.children(p)?.is_empty() {
            return Err(io(libc::ENOTEMPTY));
        }
        let mark = self
            .tombstone(p, false)?
            .map(|t| vec![(p.into(), t)])
            .unwrap_or_default();
        self.transact(Journal {
            remove: vec![p.into()],
            clear: vec![p.into()],
            mark,
            ..Default::default()
        })
    }
    pub fn materialize(&self, p: &str) -> Result<()> {
        let e = self.entry(p)?;
        if e.kind == Kind::Directory {
            let names = self.children(p)?;
            self.copy_up(p, false)?;
            for n in names {
                self.materialize(&join(p, &n))?;
            }
        } else {
            self.copy_up(p, false)?;
        }
        Ok(())
    }
    pub fn rename(&self, src: &str, dst: &str) -> Result<()> {
        self.mutable(src)?;
        self.mutable(dst)?;
        if src == dst {
            return Ok(());
        }
        if under(dst, src) {
            return Err(io(libc::EINVAL));
        }
        let from = self.entry(src)?;
        if let Ok(to) = self.entry(dst) {
            if (to.kind == Kind::Directory) != (from.kind == Kind::Directory) {
                return Err(io(libc::EINVAL));
            }
            if to.kind == Kind::Directory && !self.children(dst)?.is_empty() {
                return Err(io(libc::ENOTEMPTY));
            }
        }
        self.parents(dst)?;
        self.materialize(src)?;
        let mut mark = vec![];
        if let Some(t) = self.tombstone(src, false)? {
            mark.push((src.into(), t));
        }
        if from.kind == Kind::Directory {
            if let Some(t) = self.tombstone(dst, true)? {
                mark.push((dst.into(), t));
            }
        }
        self.transact(Journal {
            remove: vec![dst.into()],
            clear: vec![src.into(), dst.into()],
            mark,
            rename: Some((src.into(), dst.into())),
            ..Default::default()
        })
    }
    pub fn revert(&self, p: &str) -> Result<()> {
        self.mutable(p)?;
        self.transact(Journal {
            remove: vec![p.into()],
            clear: vec![p.into()],
            ..Default::default()
        })
    }
    pub fn diff(&self) -> Result<serde_json::Value> {
        let mut changes = vec![];
        for p in walk(&self.upper())? {
            let e = self.entry(&p)?;
            if e.kind == Kind::Directory {
                continue;
            }
            let equal = self.equal_base(&p, &e)?;
            if !equal {
                changes.push(serde_json::json!({"path":p,"change":if self.base.contains_key(&p){"modify"}else{"add"},"mount":self.record.map.owner(&p).map(|m|&m.id)}));
            }
        }
        for p in walk(&self.tombs())? {
            if let Some(t) = self.marker(&p)? {
                changes.push(serde_json::json!({"path":p,"change":t.kind,"mount":t.mount_id}));
            }
        }
        Ok(serde_json::json!(changes))
    }
    pub fn equal_base(&self, p: &str, e: &Entry) -> Result<bool> {
        let Some(b) = self.base.get(p) else {
            return Ok(false);
        };
        if b.kind != e.kind || e.kind == Kind::Directory {
            return Ok(false);
        }
        if e.kind == Kind::File && (e.mode & 0o111 != 0) != (b.mode & 0o111 != 0) {
            return Ok(false);
        }
        let data = if e.kind == Kind::Symlink {
            if let Some(ref f) = e.physical {
                fs::read_link(f)?.as_os_str().as_encoded_bytes().to_vec()
            } else {
                e.bytes.clone().unwrap()
            }
        } else {
            fs::read(e.physical.as_ref().unwrap())?
        };
        Ok(data == fs::read(self.base_file(p)?)?)
    }
}
