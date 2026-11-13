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
    fn state_time(&self) -> SystemTime {
        fs::metadata(self.root.join("workspace.json"))
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| SystemTime::now())
    }
    pub fn metadata(&self, p: &str) -> Result<Option<Entry>> {
        let file = |b: Vec<u8>| {
            Some(Entry {
                kind: Kind::File,
                mode: 0o444,
                size: b.len() as u64,
                time: self.state_time(),
                physical: None,
                bytes: Some(b),
            })
        };
        let link = |path: PathBuf| {
            Some(Entry {
                kind: Kind::Symlink,
                mode: 0o777,
                size: path.as_os_str().len() as u64,
                time: self.state_time(),
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
        let (parent, name) = p.rsplit_once('/').unwrap_or(("", p));
        if ![".mountgit", ".mountcommit", ".mountstate.json"].contains(&name) {
            return Ok(None);
        }
        for m in self.record.map.mounts.iter().filter(|m| m.path == parent) {
            let id = crate::util::hash(self.record.map.repositories[&m.repository].url.as_bytes());
            if p == join(&m.path, ".mountgit") {
                return Ok(link(self.state_root().join("repos").join(id)));
            }
            if p == join(&m.path, ".mountcommit") {
                return Ok(file(format!("{}\n", m.commit).into_bytes()));
            }
            if p == join(&m.path, ".mountstate.json") {
                return Ok(file(serde_json::to_vec_pretty(
                    &serde_json::json!({"format":1,"generation":self.record.generation,"mount_id":m.id,"repository":m.repository,"repository_id":id,"commit":m.commit,"subtree":m.subtree,"path":m.path,"uninitialized_submodules":m.uninitialized_submodules,"object_format":if m.commit.len()==40{"sha1"}else{"sha256"}}),
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
                time: self
                    .namespace
                    .lookup("")?
                    .as_ref()
                    .map(base_time)
                    .unwrap_or_else(|| self.state_time()),
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
        let b = self.namespace.lookup(p)?.ok_or_else(|| io(libc::ENOENT))?;
        Ok(Entry {
            kind: b.kind.clone(),
            mode: if b.kind == Kind::Directory {
                0o755
            } else {
                b.mode
            },
            size: 0,
            time: base_time(&b),
            physical: None,
            bytes: None,
        })
    }
    pub fn attributes(&self, p: &str) -> Result<Entry> {
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
            let info = self.blob_info(p)?;
            e.size = if e.kind == Kind::Symlink {
                info.size
            } else {
                info.logical_size()
            };
        }
        Ok(e)
    }
    /// Payload access is deliberately separate from stat/lookup.
    pub fn entry(&self, p: &str) -> Result<Entry> {
        let mut e = self.attributes(p)?;
        if e.physical.is_none() && e.bytes.is_none() && e.kind != Kind::Directory {
            let path = self.base_file(p)?;
            if e.kind == Kind::Symlink {
                e.bytes = Some(fs::read(path)?);
            } else {
                e.physical = Some(path);
            }
        }
        Ok(e)
    }
    fn blob_info(&self, p: &str) -> Result<crate::metadata::BlobInfo> {
        let base = self.namespace.lookup(p)?.ok_or_else(|| io(libc::ENOENT))?;
        if let Some(info) = self.metadata.get(&base.repo, &base.oid) {
            return Ok(info);
        }
        let parent = Path::new(p)
            .parent()
            .unwrap_or(Path::new(""))
            .to_str()
            .unwrap();
        let listed = self.listed.lock().unwrap();
        let enabled =
            listed.contains(parent) && !self.disabled_batches.lock().unwrap().contains(parent);
        let recursive = enabled
            && !parent.is_empty()
            && listed.contains(Path::new(parent).parent().unwrap().to_str().unwrap());
        drop(listed);
        let limit = if recursive {
            crate::metadata::TRAVERSAL_BATCH
        } else if enabled {
            crate::metadata::DIRECTORY_BATCH
        } else {
            1
        };
        let mut dirs = vec![parent.to_string()];
        let mut directory_entries = vec![];
        let preparation = (|| -> Result<()> {
            if recursive {
                let ancestor = Path::new(parent).parent().unwrap().to_str().unwrap();
                let siblings: Vec<_> = self.namespace.children(ancestor)?.into_iter().collect();
                let start = siblings.partition_point(|(s, _)| s.as_str() < parent);
                let children = self.namespace.children(parent)?;
                let candidates = children
                    .iter()
                    .chain(siblings[start..].iter().map(|(p, b)| (p, b)))
                    .chain(siblings[..start].iter().map(|(p, b)| (p, b)));
                for (path, b) in candidates {
                    if dirs.len() >= crate::metadata::TRAVERSAL_DIRS {
                        break;
                    }
                    if b.kind == Kind::Directory
                        && b.mount_id == base.mount_id
                        && !dirs.contains(path)
                    {
                        dirs.push(path.clone());
                    }
                }
                self.namespace.lookahead(&dirs)?;
            }
            for dir in &dirs {
                directory_entries.push((dir.clone(), self.namespace.children(dir)?));
            }
            Ok(())
        })();
        if let Err(e) = preparation {
            crate::activity::check()?;
            self.disabled_batches.lock().unwrap().insert(parent.into());
            crate::activity::event(
                "warn",
                &format!(
                    "Tree lookahead failed at {}:{parent}: {e}; retry demand",
                    self.record.name
                ),
            );
            directory_entries = vec![(parent.to_string(), self.namespace.children(parent)?)];
        }
        let mut wanted = vec![base.oid.clone()];
        let mut seen = BTreeSet::from([base.oid.clone()]);
        'directories: for (dir, children) in &directory_entries {
            let entries: Vec<_> = children.iter().collect();
            let start = if dir == parent {
                entries.partition_point(|(s, _)| s.as_str() < p)
            } else {
                0
            };
            for (path, b) in entries[start..].iter().chain(&entries[..start]) {
                if wanted.len() >= limit {
                    break 'directories;
                }
                crate::activity::check()?;
                if b.kind != Kind::Directory
                    && b.repo == base.repo
                    && b.mount_id == base.mount_id
                    && self.metadata.get(&b.repo, &b.oid).is_none()
                    && seen.insert(b.oid.clone())
                    && !self.hidden(path)?
                    && fs::symlink_metadata(self.safe(&self.upper(), path)?).is_err()
                {
                    wanted.push(b.oid.clone());
                }
            }
        }
        if let Err(e) =
            self.metadata
                .resolve(self.state_root(), &base.repo, &wanted, wanted.len() > 1)
        {
            crate::activity::check()?;
            if wanted.len() == 1 {
                return Err(e);
            }
            self.disabled_batches.lock().unwrap().insert(parent.into());
            crate::activity::event("warn",&format!("Attribute lookahead failed for {}:{parent}; disabling speculation and retrying requested file: {e}",self.record.name));
            self.metadata.resolve(
                self.state_root(),
                &base.repo,
                std::slice::from_ref(&base.oid),
                false,
            )?;
        }
        self.metadata
            .get(&base.repo, &base.oid)
            .ok_or_else(|| error("missing resolved blob metadata"))
    }
    pub fn base_file(&self, p: &str) -> Result<PathBuf> {
        let b = self.namespace.lookup(p)?.ok_or_else(|| io(libc::ENOENT))?;
        let target = self.state_root().join("cache").join(&b.repo).join(&b.oid);
        if target.exists() {
            return Ok(target);
        }
        let info = self.blob_info(p)?;
        let _l = Lock::take(&self.state_root().join("locks").join(&b.repo))?;
        if !target.exists() {
            crate::activity::check()?;
            crate::activity::stage(&format!("Hydrate {}: {p}", self.record.name));
            crate::activity::event("info", &format!("Hydrate {}: {p}", self.record.name));
            let repo = self.state_root().join("repos").join(&b.repo);
            let mut bytes = if let Some(small) = info.small {
                small.to_vec()
            } else {
                git::local(&repo, &["cat-file", "blob", &b.oid], None)?
            };
            if b.kind == Kind::File {
                if let Some(pointer) = crate::metadata::lfs_pointer(&bytes) {
                    let expected = pointer.oid.to_owned();
                    let size = pointer.size;
                    crate::activity::stage(&format!("LFS hydrate {}: {p}", self.record.name));
                    crate::activity::event(
                        "info",
                        &format!("LFS hydrate {}: {p}", self.record.name),
                    );
                    let mut cmd = std::process::Command::new("git");
                    cmd.arg("-C")
                        .arg(&repo)
                        .args(["lfs", "smudge"])
                        .env("GIT_TERMINAL_PROMPT", "0")
                        .env_remove("GIT_LFS_SKIP_SMUDGE")
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
                    if result.stdout.len() as u64 != size
                        || crate::util::hash(&result.stdout) != expected
                    {
                        return Err(error("LFS payload integrity mismatch"));
                    }
                    bytes = result.stdout;
                }
            }
            atomic(&target, &bytes)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o444))?;
            crate::activity::event(
                "info",
                &format!("Hydrated {}: {p} ({} bytes)", self.record.name, bytes.len()),
            );
        }
        Ok(target)
    }
    pub fn children(&self, p: &str) -> Result<Vec<String>> {
        Ok(self
            .children_entries(p)?
            .into_iter()
            .map(|(n, _)| n)
            .collect())
    }
    pub fn children_entries(&self, p: &str) -> Result<Vec<(String, Kind)>> {
        self.check()?;
        if self.direct(p)?.kind != Kind::Directory {
            return Err(io(libc::ENOTDIR));
        }
        let mut entries = std::collections::BTreeMap::new();
        if !self.hidden(p)? {
            // A depot-owned directory may replace a base file. In that case only
            // the physical directory and generated mount ancestors contribute.
            if self
                .namespace
                .lookup(p)?
                .is_some_and(|b| b.kind == Kind::Directory)
            {
                for (path, b) in self.namespace.children(p)? {
                    if !self.hidden(&path)? {
                        entries.insert(
                            Path::new(&path)
                                .file_name()
                                .unwrap()
                                .to_str()
                                .unwrap()
                                .to_owned(),
                            b.kind,
                        );
                    }
                }
            }
        }
        let upper = self.safe(&self.upper(), p)?;
        match fs::read_dir(&upper) {
            Ok(files) => {
                for file in files {
                    let file = file?;
                    let name = file
                        .file_name()
                        .into_string()
                        .map_err(|_| error("non-UTF8 filename"))?;
                    let kind = file.file_type()?;
                    entries.insert(
                        name,
                        if kind.is_dir() {
                            Kind::Directory
                        } else if kind.is_symlink() {
                            Kind::Symlink
                        } else {
                            Kind::File
                        },
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        if p.is_empty() {
            entries.insert(".depotgit".into(), Kind::Symlink);
            entries.insert(".depotstate.json".into(), Kind::File);
        }
        if self.namespace.is_mount(p) {
            entries.insert(".mountgit".into(), Kind::Symlink);
            entries.insert(".mountcommit".into(), Kind::File);
            entries.insert(".mountstate.json".into(), Kind::File);
        }
        self.listed.lock().unwrap().insert(p.into());
        Ok(entries.into_iter().collect())
    }
    pub fn mutable(&self, p: &str) -> Result<()> {
        self.check()?;
        if !valid_path(p) || self.namespace.protected(p) {
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
        let pending = match e.kind {
            Kind::Directory => self.stage(Kind::Directory, e.mode, None)?,
            Kind::Symlink => {
                use std::os::unix::ffi::OsStrExt;
                let bytes = e.bytes.ok_or("missing symlink")?;
                self.stage(
                    Kind::Symlink,
                    e.mode,
                    Some(Path::new(std::ffi::OsStr::from_bytes(&bytes))),
                )?
            }
            Kind::File => {
                let pending = self.stage(Kind::File, e.mode, None)?;
                let staged = self.root.join("pending").join(&pending);
                if !truncate {
                    fs::copy(e.physical.ok_or("missing base")?, &staged)?;
                }
                fs::set_permissions(&staged, fs::Permissions::from_mode(e.mode))?;
                File::open(staged)?.sync_all()?;
                pending
            }
        };
        self.transact(Journal {
            install: Some((pending, p.into())),
            ..Default::default()
        })?;
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
        Ok(self.namespace.lookup(p)?.and_then(|b| {
            self.namespace.owner(p).map(|m| Tombstone {
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
        if self.hidden(p)? && self.marker(p)?.is_none() {
            return Err(error(
                "path is covered by an ancestor tombstone; revert that ancestor first",
            ));
        }
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
                changes.push(serde_json::json!({"path":p,"change":if self.namespace.lookup(&p)?.is_some(){"modify"}else{"add"},"mount":self.namespace.owner(&p).map(|m|&m.id)}));
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
        let Some(b) = self.namespace.lookup(p)? else {
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

fn base_time(base: &crate::model::Base) -> SystemTime {
    let delta = std::time::Duration::from_secs(base.commit_time.unsigned_abs());
    if base.commit_time < 0 {
        SystemTime::UNIX_EPOCH.checked_sub(delta)
    } else {
        SystemTime::UNIX_EPOCH.checked_add(delta)
    }
    .unwrap_or(SystemTime::UNIX_EPOCH)
}
