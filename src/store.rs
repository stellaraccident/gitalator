use crate::{
    error, git,
    model::*,
    util::{atomic, hash, Lock},
    Result,
};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub format: u32,
    pub mount: PathBuf,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub format: u32,
    pub id: String,
    pub name: String,
    pub depot_repo: String,
    pub branch: String,
    pub active_commit: String,
    pub generation: String,
    pub map: Map,
    pub editing: bool,
    #[serde(default)]
    pub edit_token: Option<String>,
    #[serde(default)]
    pub initializing: bool,
}
/// Bounded traversal hints; scanning a forest must not retain every visited path.
#[derive(Default)]
pub struct TraversalHints {
    entries: std::collections::BTreeSet<String>,
    order: std::collections::VecDeque<String>,
}
impl TraversalHints {
    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains(path)
    }
    pub fn insert(&mut self, path: String) {
        if self.entries.contains(&path) {
            return;
        }
        if self.order.len() == 4096 {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
        self.entries.insert(path.clone());
        self.order.push_back(path);
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}
pub struct Workspace {
    pub root: PathBuf,
    pub record: Record,
    pub namespace: Arc<crate::namespace::Namespace>,
    pub handles: usize,
    pub listed: Mutex<TraversalHints>,
    pub disabled_batches: Mutex<TraversalHints>,
    pub metadata: Arc<crate::metadata::Metadata>,
}
#[derive(Clone)]
pub struct Store {
    pub metadata: Arc<crate::metadata::Metadata>,
    pub trees: Arc<crate::trees::Trees>,
    pub root: PathBuf,
    pub config: Config,
    pub workspaces: BTreeMap<String, Arc<Mutex<Workspace>>>,
}
pub fn token() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}
pub fn default_root() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".gitalator_state")
}
pub fn absolute(p: &Path) -> Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.into())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}
pub fn json_write<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    atomic(p, &serde_json::to_vec_pretty(v)?)
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        if root.join("state.sqlite3").exists() {
            return Err(error("legacy SQLite state is unsupported; choose a fresh --state-dir (old data was not modified)"));
        }
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let config = if root.join("config.json").exists() {
            serde_json::from_slice::<Config>(&fs::read(root.join("config.json"))?)?
        } else {
            let def = absolute(&default_root())?;
            let mount = if root == def {
                def.parent().unwrap().join("ws")
            } else {
                root.join("mount")
            };
            let c = Config { format: 1, mount };
            json_write(&root.join("config.json"), &c)?;
            c
        };
        if config.format != 1 || !config.mount.is_absolute() {
            return Err(error("invalid state configuration"));
        }
        for d in ["repos", "workspaces", "locks", "cache", "logs"] {
            fs::create_dir_all(root.join(d))?;
        }
        let mut s = Self {
            metadata: Default::default(),
            trees: Arc::new(crate::trees::Trees::new()?),
            root,
            config,
            workspaces: BTreeMap::new(),
        };
        for entry in fs::read_dir(s.root.join("workspaces"))? {
            let p = entry?.path();
            if !p.join("workspace.json").exists() {
                continue;
            }
            let record: Record = serde_json::from_slice(&fs::read(p.join("workspace.json"))?)?;
            if record.format != 1 || !name(&record.name) || s.workspaces.contains_key(&record.name)
            {
                return Err(error("invalid workspace record"));
            }
            let mut w = Workspace {
                root: p,
                record,
                namespace: Arc::new(crate::namespace::Namespace::empty(
                    s.root.clone(),
                    s.trees.clone(),
                )),
                handles: 0,
                listed: Default::default(),
                disabled_batches: Default::default(),
                metadata: s.metadata.clone(),
            };
            let started = std::time::Instant::now();
            index_progress(&format!(
                "Open workspace {}: recover journals",
                w.record.name
            ));
            w.recover()?;
            w.recover_layout()?;
            w.namespace = s.prepare_namespace(&w.record.map)?;
            index_progress(&format!(
                "Workspace {}: validate depot overlay and tombstones",
                w.record.name
            ));
            if w.record.initializing {
                let recovery = s.materialize_depot(&w).and_then(|_| w.validate());
                match recovery {
                    Ok(()) => {
                        w.record.initializing = false;
                        w.record.editing = false;
                        w.record.edit_token = None;
                    }
                    Err(e) => eprintln!(
                        "gtl: workspace {} initialization remains suspended: {e}",
                        w.record.name
                    ),
                }
            }
            if !w.record.editing {
                w.validate()?;
            }
            w.record.generation = token();
            w.save()?;
            index_progress(&format!(
                "Workspace {} opened in {:.2}s",
                w.record.name,
                started.elapsed().as_secs_f64()
            ));
            s.workspaces
                .insert(w.record.name.clone(), Arc::new(Mutex::new(w)));
        }
        Ok(s)
    }
    pub fn repo(&self, url: &str) -> Result<String> {
        let id = hash(url.as_bytes());
        let p = self.root.join("repos").join(&id);
        let _l = Lock::take(&self.root.join("locks").join(&id))?;
        if !p.exists() {
            fs::create_dir(&p)?;
        }
        if !p.join("HEAD").exists() {
            git::run(&p, &["init", "--bare"])?;
            git::run(&p, &["remote", "add", "origin", url])?;
        }
        git::run(&p, &["config", "remote.origin.promisor", "true"])?;
        git::run(
            &p,
            &["config", "remote.origin.partialclonefilter", "blob:none"],
        )?;
        Ok(id)
    }
    pub fn fetch(&self, id: &str, revision: &str) -> Result<String> {
        if revision.starts_with('-') || revision.is_empty() {
            return Err(error("invalid revision"));
        }
        let _l = Lock::take(&self.root.join("locks").join(id))?;
        let p = self.root.join("repos").join(id);
        if oid(revision)
            && git::local(
                &p,
                &["cat-file", "-e", &format!("{revision}^{{commit}}")],
                None,
            )
            .is_ok()
        {
            return Ok(revision.into());
        }
        git::run(
            &p,
            &[
                "fetch",
                "--filter=blob:none",
                "--recurse-submodules=no",
                "--no-tags",
                "--no-auto-maintenance",
                "origin",
                revision,
            ],
        )?;
        git::text(&p, &["rev-parse", "FETCH_HEAD^{commit}"])
    }
    /// New local depots live directly in the managed bare store, without a
    /// temporary checkout, synthetic remote, or a second copy of the repository.
    pub fn init(&mut self, name_: &str) -> Result<String> {
        if !name(name_) || self.workspaces.contains_key(name_) {
            return Err(error("invalid or existing workspace name"));
        }
        let repo = hash(format!("local-depot:{}", token()).as_bytes());
        let bare = self.root.join("repos").join(&repo);
        fs::create_dir(&bare)?;
        let _lock = Lock::take(&self.root.join("locks").join(&repo))?;
        git::run(&bare, &["init", "--bare", "--initial-branch=main"])?;
        let blob = String::from_utf8(git::input(
            &bare,
            &["hash-object", "-w", "--stdin"],
            Some(b"format = 1\nmounts = []\n\n[repositories]\n"),
        )?)?
        .trim()
        .to_owned();
        let tree = String::from_utf8(git::input(
            &bare,
            &["mktree"],
            Some(format!("100644 blob {blob}\tdepot.toml\n").as_bytes()),
        )?)?
        .trim()
        .to_owned();
        // Only the bootstrap commit uses this identity. User commits still use
        // the user's normal Git configuration.
        let commit = git::text(
            &bare,
            &[
                "-c",
                "user.name=Gitalator",
                "-c",
                "user.email=gitalator@localhost",
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                &tree,
                "-m",
                "Initialize empty depot",
            ],
        )?;
        git::run(&bare, &["update-ref", "refs/heads/main", &commit])?;
        drop(_lock);
        self.create_from_repo(name_, repo, commit)
    }
    pub fn create(&mut self, name_: &str, url: &str, revision: &str) -> Result<String> {
        if !name(name_) || self.workspaces.contains_key(name_) {
            return Err(error("invalid or existing workspace name"));
        }
        let repo = self.repo(url)?;
        let commit = self.fetch(&repo, revision)?;
        self.create_from_repo(name_, repo, commit)
    }
    pub(crate) fn create_from_repo(
        &mut self,
        name_: &str,
        repo: String,
        commit: String,
    ) -> Result<String> {
        if !name(name_) || self.workspaces.contains_key(name_) {
            return Err(error("invalid or existing workspace name"));
        }
        let bare = self.root.join("repos").join(&repo);
        let map = Map::parse(&String::from_utf8(git::run(
            &bare,
            &["show", &format!("{commit}:depot.toml")],
        )?)?)?;
        let namespace = self.prepare_namespace(&map)?;
        let id = token();
        let root = self.root.join("workspaces").join(&id);
        fs::create_dir(&root)?;
        let branch = format!("workspaces/{id}");
        let mut w = Workspace {
            root,
            record: Record {
                format: 1,
                id,
                name: name_.into(),
                depot_repo: repo,
                branch,
                active_commit: commit,
                generation: token(),
                map,
                editing: true,
                edit_token: Some(token()),
                initializing: true,
            },
            metadata: self.metadata.clone(),
            namespace,
            handles: 0,
            listed: Default::default(),
            disabled_batches: Default::default(),
        };
        w.save()?;
        crate::failpoint("workspace-record");
        if let Err(e) = self.materialize_depot(&w) {
            self.workspaces
                .insert(name_.into(), Arc::new(Mutex::new(w)));
            return Err(e);
        }
        crate::failpoint("workspace-materialized");
        fs::create_dir_all(w.upper())?;
        fs::create_dir_all(w.tombs())?;
        if let Err(e) = w.validate() {
            w.record.editing = true;
            w.save()?;
            self.workspaces
                .insert(name_.into(), Arc::new(Mutex::new(w)));
            return Err(e);
        }
        w.record.initializing = false;
        w.record.editing = false;
        w.record.edit_token = None;
        w.save()?;
        self.workspaces
            .insert(name_.into(), Arc::new(Mutex::new(w)));
        Ok(self.config.mount.join(name_).display().to_string())
    }
    fn materialize_depot(&self, w: &Workspace) -> Result<()> {
        if w.depot().join(".git").is_file() {
            return Ok(());
        }
        let bare = self.root.join("repos").join(&w.record.depot_repo);
        let _l = Lock::take(&self.root.join("locks").join(&w.record.depot_repo))?;
        if git::run(
            &bare,
            &[
                "show-ref",
                "--verify",
                &format!("refs/heads/{}", w.record.branch),
            ],
        )
        .is_ok()
        {
            git::run(
                &bare,
                &[
                    "worktree",
                    "add",
                    w.depot().to_str().unwrap(),
                    &w.record.branch,
                ],
            )?;
        } else {
            git::run(
                &bare,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &w.record.branch,
                    w.depot().to_str().unwrap(),
                    &w.record.active_commit,
                ],
            )?;
        }
        Ok(())
    }
    pub fn get(&self, n: &str) -> Result<Arc<Mutex<Workspace>>> {
        self.workspaces
            .get(n)
            .cloned()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT).into())
    }
}
impl Workspace {
    pub fn depot(&self) -> PathBuf {
        self.root.join("depot")
    }
    pub fn upper(&self) -> PathBuf {
        self.depot().join("overlay")
    }
    pub fn tombs(&self) -> PathBuf {
        self.depot().join("tombstones")
    }
    pub fn save(&self) -> Result<()> {
        json_write(&self.root.join("workspace.json"), &self.record)
    }
    pub fn check(&self) -> Result<()> {
        if self.record.editing
            || self.root.join("journal.json").exists()
            || self.root.join("layout.json").exists()
        {
            return Err(std::io::Error::from_raw_os_error(libc::EBUSY).into());
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<()> {
        if !fs::symlink_metadata(self.depot().join("depot.toml"))?.is_file() {
            return Err(error("depot.toml must be a regular file"));
        }
        let map = Map::parse(&fs::read_to_string(self.depot().join("depot.toml"))?)?;
        if map != self.record.map {
            return Err(error(
                "manual pin/layout changes are unsupported; use mount commands for supported layout changes or restore the active depot.toml",
            ));
        }
        self.validate_content()
    }
    pub(crate) fn validate_content(&self) -> Result<()> {
        self.record.map.validate()?;
        self.namespace.validate_boundaries()?;
        for root in [self.upper(), self.tombs()] {
            if let Ok(m) = fs::symlink_metadata(root) {
                if !m.is_dir() {
                    return Err(error("backing tree must be a physical directory"));
                }
            }
        }
        for p in walk(&self.upper())? {
            if !valid_path(&p) || self.namespace.reserved(&p) {
                return Err(error("invalid overlay path"));
            }
            self.namespace.lookup(&p)?;
            let m = fs::symlink_metadata(self.upper().join(&p))?;
            if !m.is_dir() && self.namespace.protected(&p) {
                return Err(error("overlay would hide a mount root"));
            }
            if !m.is_file() && !m.is_dir() && !m.file_type().is_symlink() {
                return Err(error("unsupported upper file type"));
            }
        }
        for p in walk(&self.tombs())? {
            let f = self.tombs().join(&p);
            if fs::symlink_metadata(&f)?.is_dir() {
                continue;
            }
            if !fs::symlink_metadata(&f)?.is_file() || !valid_path(&p) {
                return Err(error("invalid tombstone path"));
            }
            let t: Tombstone = serde_json::from_slice(&fs::read(&f)?)?;
            let m = self.namespace.owner(&p).ok_or("tombstone outside mount")?;
            let base = self
                .namespace
                .lookup(&p)?
                .ok_or("tombstone has no base entry")?;
            if t.base_oid != base.oid || t.base_kind != base.kind {
                return Err(error("tombstone base identity mismatch"));
            }
            if t.format != 1
                || t.mount_id != m.id
                || t.base_commit != m.commit
                || !matches!(t.kind.as_str(), "delete" | "opaque")
                || self.namespace.protected(&p)
            {
                return Err(error("invalid tombstone provenance"));
            }
            if let Ok(meta) = fs::symlink_metadata(self.upper().join(&p)) {
                if t.kind != "opaque" || !meta.is_dir() {
                    return Err(error("contradictory payload and tombstone"));
                }
            }
        }
        Ok(())
    }
    pub fn recover(&mut self) -> Result<()> {
        let p = self.root.join("journal.json");
        if p.exists() {
            let j: Journal = serde_json::from_slice(&fs::read(&p)?)?;
            self.apply(&j)?;
            fs::remove_file(&p)?;
        }
        Ok(())
    }
    fn apply(&self, j: &Journal) -> Result<()> {
        // Redo a metadata/namespace operation after a crash. Payloads are real files.
        let renamed = if let Some((src, _)) = &j.rename {
            fs::symlink_metadata(self.safe(&self.upper(), src)?).is_err()
        } else {
            false
        };
        for p in &j.remove {
            if renamed && j.rename.as_ref().is_some_and(|(_, dst)| dst == p) {
                continue;
            }
            let dst = self.safe(&self.upper(), p)?;
            remove_any(&dst)?;
        }
        for p in &j.clear {
            match self.safe(&self.tombs(), p) {
                Ok(dst) => remove_any(&dst)?,
                Err(e)
                    if e.downcast_ref::<std::io::Error>()
                        .and_then(|e| e.raw_os_error())
                        == Some(libc::ENOTDIR) => {}
                Err(e) => return Err(e),
            }
        }
        for (p, t) in &j.mark {
            let dst = self.safe(&self.tombs(), p)?;
            remove_any(&dst)?;
            json_write(&dst, t)?;
        }
        if let Some((src, dst)) = &j.rename {
            let a = self.safe(&self.upper(), src)?;
            let b = self.safe(&self.upper(), dst)?;
            if a.symlink_metadata().is_ok() {
                fs::create_dir_all(b.parent().unwrap())?;
                fs::rename(a, b)?;
            }
        }
        if let Some((pending, dst)) = &j.install {
            if !name(pending) {
                return Err(error("invalid pending payload"));
            }
            let src = self.root.join("pending").join(pending);
            let dst = self.safe(&self.upper(), dst)?;
            if fs::symlink_metadata(&src).is_ok() {
                fs::create_dir_all(dst.parent().unwrap())?;
                fs::rename(src, dst)?;
            }
        }
        // Sync only touched parent chains, not the entire sparse worktree.
        let paths: Vec<&str> = j
            .remove
            .iter()
            .chain(&j.clear)
            .map(String::as_str)
            .chain(j.mark.iter().map(|(p, _)| p.as_str()))
            .chain(j.rename.iter().flat_map(|(a, b)| [a.as_str(), b.as_str()]))
            .chain(j.install.iter().map(|(_, p)| p.as_str()))
            .collect();
        for root in [self.upper(), self.tombs()] {
            for p in &paths {
                let full = root.join(p);
                let mut current = Some(full.as_path());
                while let Some(dir) = current {
                    if dir.starts_with(&root) && fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
                    {
                        fs::File::open(dir)?.sync_all()?;
                    }
                    if dir == root {
                        break;
                    }
                    current = dir.parent();
                }
            }
        }
        Ok(())
    }
    pub fn transact(&self, j: Journal) -> Result<()> {
        let p = self.root.join("journal.json");
        if p.exists() {
            return Err(error(
                "pending recovery; restart the daemon before more mutations",
            ));
        }
        crate::activity::check()?;
        let _critical = crate::activity::critical();
        json_write(&p, &j)?;
        crate::failpoint("journal-published");
        self.apply(&j)?;
        crate::failpoint("journal-applied");
        fs::remove_file(p)?;
        fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn safe(&self, root: &Path, p: &str) -> Result<PathBuf> {
        if !valid_path(p) {
            return Err(error("invalid path"));
        }
        let mut cur = root.to_path_buf();
        if let Ok(m) = fs::symlink_metadata(root) {
            if !m.is_dir() {
                return Err(error("invalid backing tree"));
            }
        }
        let parts: Vec<_> = p.split('/').filter(|x| !x.is_empty()).collect();
        for (i, c) in parts.iter().enumerate() {
            cur.push(c);
            if i + 1 < parts.len() {
                if let Ok(m) = fs::symlink_metadata(&cur) {
                    if !m.is_dir() {
                        return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR).into());
                    }
                }
            }
        }
        Ok(cur)
    }
}
#[derive(Default, Serialize, Deserialize)]
pub struct Journal {
    pub remove: Vec<String>,
    pub clear: Vec<String>,
    pub mark: Vec<(String, Tombstone)>,
    pub rename: Option<(String, String)>,
    #[serde(default)]
    pub install: Option<(String, String)>,
}
pub fn remove_any(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p)?,
        Ok(_) => fs::remove_file(p)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
pub fn walk(root: &Path) -> Result<Vec<String>> {
    fn go(root: &Path, p: &Path, out: &mut Vec<String>) -> Result<()> {
        for e in fs::read_dir(p)? {
            let e = e?;
            let path = e.path();
            out.push(
                path.strip_prefix(root)?
                    .to_str()
                    .ok_or("non-UTF8 filenames are not supported")?
                    .into(),
            );
            if e.file_type()?.is_dir() {
                go(root, &path, out)?;
            }
        }
        Ok(())
    }
    let mut out = vec![];
    if root.exists() {
        go(root, root, &mut out)?;
    }
    out.sort();
    Ok(out)
}
pub fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for p in walk(src)? {
        let a = src.join(&p);
        let b = dst.join(&p);
        let m = fs::symlink_metadata(&a)?;
        if m.is_dir() {
            fs::create_dir_all(&b)?;
        } else if m.file_type().is_symlink() {
            symlink(fs::read_link(a)?, b)?;
        } else if m.is_file() {
            fs::copy(a, b)?;
        } else {
            return Err(error("unsupported file"));
        }
    }
    Ok(())
}

fn index_progress(message: &str) {
    crate::activity::stage(message);
    crate::activity::event("info", message);
}

pub(crate) fn index_jobs() -> Result<usize> {
    match std::env::var("GTL_INDEX_JOBS") {
        Ok(value) => value
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=64).contains(n))
            .ok_or_else(|| error("GTL_INDEX_JOBS must be an integer from 1 through 64")),
        Err(std::env::VarError::NotPresent) => Ok(std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(8)),
        Err(_) => Err(error("GTL_INDEX_JOBS must be an integer from 1 through 64")),
    }
}
