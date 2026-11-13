//! Mount-aware lazy base namespace. No flattened per-workspace file index.
use crate::{
    activity, error,
    model::*,
    store::Store,
    trees::{self, Trees},
    util::{atomic, hash},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Commit {
    format: u32,
    repo: String,
    commit: String,
    pub(crate) tree: String,
    time: i64,
}
#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    mount: Option<usize>,
    time: Option<i64>,
}
pub struct Namespace {
    root: PathBuf,
    map: Map,
    roots: Vec<Base>,
    trie: Node,
    pub trees: Arc<Trees>,
}
impl Store {
    pub fn prepare_namespace(&self, map: &Map) -> Result<Arc<Namespace>> {
        map.validate()?;
        let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for m in &map.mounts {
            groups
                .entry(map.repositories[&m.repository].url.clone())
                .or_default()
                .insert(m.commit.clone());
        }
        let groups: Vec<_> = groups.into_iter().collect();
        let jobs = crate::store::index_jobs()?;
        activity::stage(&format!(
            "Prepare {} mount roots from {} repositories",
            map.mounts.len(),
            groups.len()
        ));
        activity::event(
            "info",
            &format!(
                "Prepare {} mount roots; {jobs} workers; no recursive index",
                map.mounts.len()
            ),
        );
        let mut commits = BTreeMap::new();
        for window in groups.chunks(jobs) {
            activity::check()?;
            let scope = activity::capture();
            let results = std::thread::scope(|threads| {
                let workers: Vec<_> = window
                    .iter()
                    .map(|(url, pins)| {
                        let scope = scope.clone();
                        threads.spawn(move || {
                            let _context = scope.enter();
                            self.prepare_commits(url, pins)
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|w| {
                        w.join()
                            .unwrap_or_else(|_| Err(error("root preparation worker panicked")))
                    })
                    .collect::<Vec<_>>()
            });
            for group in results {
                for c in group? {
                    commits.insert((c.repo.clone(), c.commit.clone()), c);
                }
            }
        }
        let mut namespace = Namespace::empty(self.root.clone(), self.trees.clone());
        namespace.map = map.clone();
        for (index, m) in map.mounts.iter().enumerate() {
            activity::check()?;
            let repo = hash(map.repositories[&m.repository].url.as_bytes());
            let c = &commits[&(repo.clone(), m.commit.clone())];
            let mut tree = c.tree.clone();
            for part in m.subtree.split('/').filter(|p| !p.is_empty()) {
                let entries = self.trees.get(&self.root, &repo, &tree)?;
                let entry = entries
                    .get(part)
                    .filter(|e| e.mode == 0o40000)
                    .ok_or("mount subtree is not a directory")?;
                tree = entry.oid.clone();
            }
            namespace.roots.push(Base {
                commit_time: c.time,
                kind: Kind::Directory,
                mode: 0o755,
                oid: tree,
                repo,
                mount_id: m.id.clone(),
            });
            let mut node = &mut namespace.trie;
            node.time.get_or_insert(c.time);
            for part in m.path.split('/').filter(|p| !p.is_empty()) {
                node = node.children.entry(part.into()).or_default();
                node.time.get_or_insert(c.time);
            }
            node.mount = Some(index);
        }
        namespace.validate_boundaries()?;
        activity::event(
            "info",
            &format!("Mount roots prepared; tree cache {}", self.trees.stats()),
        );
        Ok(Arc::new(namespace))
    }
    pub(crate) fn prepare_commits(
        &self,
        url: &str,
        pins: &BTreeSet<String>,
    ) -> Result<Vec<Commit>> {
        let repo = hash(url.as_bytes());
        let directory = self.root.join("tree-cache/v1/commits").join(&repo);
        let mut result = vec![];
        let mut missing = vec![];
        for pin in pins {
            let cached = fs::read(directory.join(format!("{pin}.json")))
                .ok()
                .and_then(|b| serde_json::from_slice::<Commit>(&b).ok());
            if let Some(c) = cached.filter(|c| {
                c.format == 1
                    && c.repo == repo
                    && c.commit == *pin
                    && oid(&c.tree)
                    && self.root.join("repos").join(&repo).join("HEAD").is_file()
            }) {
                result.push(c);
            } else {
                missing.push(pin.clone());
            }
        }
        if !missing.is_empty() {
            self.repo(url)?;
            for pin in &missing {
                self.fetch(&repo, pin)?;
            }
            for chunk in missing.chunks(128) {
                let bodies = trees::objects(&self.root.join("repos").join(&repo), chunk, "commit")?;
                for (pin, body) in chunk.iter().zip(bodies) {
                    let header = body
                        .split(|b| *b == b'\n')
                        .take_while(|line| !line.is_empty());
                    let (mut tree, mut time) = (None, None);
                    for line in header {
                        if let Some(value) = line.strip_prefix(b"tree ") {
                            tree = Some(std::str::from_utf8(value)?.to_string());
                        }
                        if let Some(value) = line.strip_prefix(b"committer ") {
                            time = value
                                .rsplit(|b| *b == b' ')
                                .nth(1)
                                .and_then(|s| std::str::from_utf8(s).ok())
                                .and_then(|s| s.parse::<i64>().ok());
                        }
                    }
                    let tree = tree.filter(|t| oid(t)).ok_or("invalid commit root tree")?;
                    let c = Commit {
                        format: 1,
                        repo: repo.clone(),
                        commit: pin.clone(),
                        tree,
                        time: time.ok_or("invalid commit time")?,
                    };
                    atomic(
                        &directory.join(format!("{pin}.json")),
                        &serde_json::to_vec(&c)?,
                    )?;
                    result.push(c);
                }
            }
        }
        Ok(result)
    }
}
impl Namespace {
    pub fn empty(root: PathBuf, trees: Arc<Trees>) -> Self {
        Self {
            root,
            trees,
            map: Map {
                format: 1,
                ..Default::default()
            },
            roots: vec![],
            trie: Node::default(),
        }
    }
    fn node(&self, path: &str) -> Option<&Node> {
        let mut node = &self.trie;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            node = node.children.get(part)?;
        }
        Some(node)
    }
    fn owner_index(&self, path: &str) -> Option<usize> {
        let mut node = &self.trie;
        let mut owner = node.mount;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            let Some(child) = node.children.get(part) else {
                break;
            };
            node = child;
            if node.mount.is_some() {
                owner = node.mount;
            }
        }
        owner
    }
    pub fn owner(&self, path: &str) -> Option<&Mount> {
        self.owner_index(path).map(|i| &self.map.mounts[i])
    }
    pub fn is_mount(&self, path: &str) -> bool {
        self.node(path).is_some_and(|n| n.mount.is_some())
    }
    pub fn reserved(&self, path: &str) -> bool {
        let mut parent = String::new();
        for part in path.split('/') {
            if (parent.is_empty() && matches!(part, ".depotgit" | ".depotstate.json"))
                || (self.is_mount(&parent)
                    && matches!(part, ".mountgit" | ".mountcommit" | ".mountstate.json"))
            {
                return true;
            }
            parent = join(&parent, part);
        }
        false
    }
    pub fn protected(&self, path: &str) -> bool {
        self.node(path).is_some() || self.reserved(path)
    }
    fn context(
        &self,
        path: &str,
        e: Box<dyn std::error::Error + Send + Sync>,
    ) -> Box<dyn std::error::Error + Send + Sync> {
        if activity::interrupted(&*e) || e.downcast_ref::<std::io::Error>().is_some() {
            e
        } else {
            let message = format!(
                "namespace path {path:?}, mount {}: {e}",
                self.owner(path)
                    .map(|m| m.id.as_str())
                    .unwrap_or("<synthetic>")
            );
            activity::event("error", &message);
            error(message)
        }
    }
    pub fn lookup(&self, path: &str) -> Result<Option<Base>> {
        activity::check()?;
        self.lookup_inner(path).map_err(|e| self.context(path, e))
    }
    fn lookup_inner(&self, path: &str) -> Result<Option<Base>> {
        if !valid_path(path) {
            return Err(error("unsupported namespace path"));
        }
        if let Some(index) = self.owner_index(path) {
            if let Some(base) = self.raw_lookup(index, path)? {
                return Ok(Some(base));
            }
        }
        Ok(self.node(path).map(|n| Base {
            commit_time: n.time.unwrap_or(0),
            kind: Kind::Directory,
            mode: 0o755,
            oid: String::new(),
            repo: String::new(),
            mount_id: String::new(),
        }))
    }
    fn convert(&self, index: usize, path: &str, e: &trees::TreeEntry) -> Result<Base> {
        let m = &self.map.mounts[index];
        let kind = match e.mode {
            0o40000 => Kind::Directory,
            0o100644 | 0o100755 => Kind::File,
            0o120000 => Kind::Symlink,
            0o160000 if m.uninitialized_submodules => Kind::Directory,
            0o160000 => {
                return Err(error(format!(
                "gitlink at {path} requires an explicit mount or uninitialized-submodules policy"
            )))
            }
            _ => return Err(error("unsupported tree mode")),
        };
        let root = &self.roots[index];
        Ok(Base {
            kind,
            mode: if e.mode == 0o160000 {
                e.mode
            } else if e.mode == 0o100755 || e.mode == 0o40000 {
                0o755
            } else {
                0o644
            },
            oid: e.oid.clone(),
            repo: root.repo.clone(),
            mount_id: m.id.clone(),
            commit_time: root.commit_time,
        })
    }
    fn raw_lookup(&self, index: usize, path: &str) -> Result<Option<Base>> {
        let m = &self.map.mounts[index];
        let suffix = if m.path.is_empty() {
            path
        } else {
            path.strip_prefix(&m.path).unwrap().trim_start_matches('/')
        };
        let mut base = self.roots[index].clone();
        let mut full = m.path.clone();
        for part in suffix.split('/').filter(|p| !p.is_empty()) {
            if base.kind != Kind::Directory {
                return Ok(None);
            }
            if base.mode == 0o160000 {
                return Ok(None);
            }
            let entries = self.trees.get(&self.root, &base.repo, &base.oid)?;
            let Some(entry) = entries.get(part) else {
                return Ok(None);
            };
            full = join(&full, part);
            base = self.convert(index, &full, entry)?;
        }
        Ok(Some(base))
    }
    pub fn children(&self, path: &str) -> Result<BTreeMap<String, Base>> {
        activity::check()?;
        self.children_inner(path).map_err(|e| self.context(path, e))
    }
    fn children_inner(&self, path: &str) -> Result<BTreeMap<String, Base>> {
        let base = self
            .lookup(path)?
            .ok_or_else(|| crate::overlay::io(libc::ENOENT))?;
        if base.kind != Kind::Directory {
            return Err(crate::overlay::io(libc::ENOTDIR));
        }
        let mut result = BTreeMap::new();
        if !base.oid.is_empty() && base.mode != 0o160000 {
            let index = self.owner_index(path).unwrap();
            let tree = self.trees.get(&self.root, &base.repo, &base.oid)?;
            for (name, e) in tree.iter() {
                let child = join(path, name);
                if self.reserved(&child) {
                    return Err(error(format!(
                        "component collides with generated metadata: {child}"
                    )));
                }
                if self.is_mount(&child) {
                    result.insert(child.clone(), self.lookup(&child)?.unwrap());
                } else {
                    result.insert(child.clone(), self.convert(index, &child, e)?);
                }
            }
        }
        if let Some(node) = self.node(path) {
            for name in node.children.keys() {
                let child = join(path, name);
                if let std::collections::btree_map::Entry::Vacant(e) = result.entry(child.clone()) {
                    e.insert(self.lookup(&child)?.unwrap());
                }
            }
        }
        Ok(result)
    }
    pub fn validate_boundaries(&self) -> Result<()> {
        for m in &self.map.mounts {
            activity::check()?;
            let mut parent = Path::new(&m.path).parent();
            while let Some(p) = parent {
                if self
                    .lookup(p.to_str().unwrap())?
                    .is_some_and(|b| b.kind != Kind::Directory)
                {
                    return Err(error("mount parent is not a directory"));
                }
                parent = p.parent();
            }
            let index = self.owner_index(&m.path).unwrap();
            for name in [".mountgit", ".mountcommit", ".mountstate.json"] {
                if self.raw_lookup(index, &join(&m.path, name))?.is_some() {
                    return Err(error(
                        "component collides with generated discovery metadata",
                    ));
                }
            }
        }
        if let Some(index) = self.owner_index("") {
            for name in [".depotgit", ".depotstate.json"] {
                if self.raw_lookup(index, name)?.is_some() {
                    return Err(error(
                        "component collides with generated discovery metadata",
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn lookahead(&self, paths: &[String]) -> Result<()> {
        let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for path in paths.iter().take(32) {
            if let Some(b) = self.lookup(path)? {
                if b.kind == Kind::Directory && !b.oid.is_empty() && b.mode != 0o160000 {
                    groups.entry(b.repo).or_default().insert(b.oid);
                }
            }
        }
        for (repo, ids) in groups {
            self.trees.batch(
                &self.root,
                &repo,
                &ids.into_iter().collect::<Vec<_>>(),
                true,
            )?;
        }
        Ok(())
    }
    pub fn validate_full(&self) -> Result<()> {
        let jobs = crate::store::index_jobs()?;
        for window in self.map.mounts.chunks(jobs) {
            let scope = activity::capture();
            let results = std::thread::scope(|threads| {
                let workers: Vec<_> = window
                    .iter()
                    .map(|m| {
                        let scope = scope.clone();
                        threads.spawn(move || {
                            let _context = scope.enter();
                            let mut todo = std::collections::VecDeque::from([m.path.clone()]);
                            let mut count = 0usize;
                            while !todo.is_empty() {
                                activity::check()?;
                                let batch: Vec<_> = (0..todo.len().min(32))
                                    .filter_map(|_| todo.pop_front())
                                    .collect();
                                // Prime the known frontier in one object batch. On a
                                // speculative error, demand each directory to report
                                // its exact path rather than hide or cache the failure.
                                if self.lookahead(&batch).is_err() {
                                    activity::check()?;
                                }
                                for path in batch {
                                    for (p, b) in self.children(&path)? {
                                        if b.kind == Kind::Directory
                                            && b.mount_id == m.id
                                            && b.mode != 0o160000
                                        {
                                            todo.push_back(p);
                                        }
                                    }
                                    count += 1;
                                }
                                activity::stage(&format!(
                                    "Full validation {}: {count} directories",
                                    m.id
                                ));
                            }
                            activity::event(
                                "info",
                                &format!("Validated mount {}: {count} directories", m.id),
                            );
                            Ok(())
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|w| {
                        w.join()
                            .unwrap_or_else(|_| Err(error("validation worker panicked")))
                    })
                    .collect::<Vec<Result<()>>>()
            });
            for result in results {
                result?;
            }
        }
        Ok(())
    }
}
