//! Immutable immediate-directory objects shared across workspace generations.
use crate::{
    activity, error, git,
    metadata::Gate,
    model::{oid, valid_path},
    util::{atomic, hash},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeEntry {
    pub mode: u32,
    pub oid: String,
}
pub type Tree = BTreeMap<String, TreeEntry>;
type Key = (String, String);
struct Cached {
    tree: Arc<Tree>,
    size: usize,
    stamp: u64,
}
#[derive(Default)]
struct Memory {
    entries: HashMap<Key, Cached>,
    order: BTreeMap<u64, Key>,
    bytes: usize,
    clock: u64,
}
pub struct Trees {
    memory: Mutex<Memory>,
    gates: Mutex<HashMap<String, Arc<Gate>>>,
    budget: usize,
    loads: AtomicU64,
    hits: AtomicU64,
}
#[derive(Serialize, Deserialize)]
struct DiskTree {
    format: u32,
    repo: String,
    oid: String,
    checksum: String,
    entries: Tree,
}
impl Trees {
    pub fn new() -> Result<Self> {
        let mb = match std::env::var("GTL_TREE_CACHE_MB") {
            Ok(s) => s
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=65536).contains(n))
                .ok_or("GTL_TREE_CACHE_MB must be an integer from 1 through 65536")?,
            Err(std::env::VarError::NotPresent) => 128,
            Err(_) => return Err(error("invalid GTL_TREE_CACHE_MB")),
        };
        Ok(Self::with_budget(mb * 1024 * 1024))
    }
    pub fn with_budget(budget: usize) -> Self {
        Self {
            memory: Mutex::new(Memory::default()),
            gates: Mutex::new(HashMap::new()),
            budget,
            loads: AtomicU64::new(0),
            hits: AtomicU64::new(0),
        }
    }
    pub fn stats(&self) -> serde_json::Value {
        let m = self.memory.lock().unwrap();
        serde_json::json!({"tree_objects_loaded":self.loads.load(Ordering::Relaxed),"cache_hits":self.hits.load(Ordering::Relaxed),"resident_bytes":m.bytes,"budget_bytes":self.budget,"resident_trees":m.entries.len()})
    }
    fn cached(&self, key: &Key) -> Option<Arc<Tree>> {
        let mut m = self.memory.lock().unwrap();
        m.clock += 1;
        let stamp = m.clock;
        let entry = m.entries.get_mut(key)?;
        let old = entry.stamp;
        entry.stamp = stamp;
        let tree = entry.tree.clone();
        m.order.remove(&old);
        m.order.insert(stamp, key.clone());
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(tree)
    }
    fn remember(&self, key: Key, tree: Arc<Tree>) {
        let size = 128
            + key.0.len()
            + key.1.len()
            + tree
                .iter()
                .map(|(n, e)| 128 + n.capacity() + e.oid.capacity())
                .sum::<usize>();
        if size > self.budget {
            return;
        }
        let mut m = self.memory.lock().unwrap();
        if let Some(old) = m.entries.remove(&key) {
            m.bytes -= old.size;
            m.order.remove(&old.stamp);
        }
        while m.bytes + size > self.budget {
            let Some((_, oldest)) = m.order.pop_first() else {
                break;
            };
            if let Some(old) = m.entries.remove(&oldest) {
                m.bytes -= old.size;
            }
        }
        m.clock += 1;
        let stamp = m.clock;
        m.order.insert(stamp, key.clone());
        m.bytes += size;
        m.entries.insert(key, Cached { tree, size, stamp });
    }
    pub fn get(&self, root: &Path, repo: &str, id: &str) -> Result<Arc<Tree>> {
        self.batch(root, repo, &[id.to_owned()], false)?
            .pop()
            .ok_or_else(|| error("missing tree result"))
    }
    pub fn batch(
        &self,
        root: &Path,
        repo: &str,
        ids: &[String],
        speculative: bool,
    ) -> Result<Vec<Arc<Tree>>> {
        activity::check()?;
        if ids.iter().any(|id| !oid(id)) {
            return Err(error("invalid tree object ID"));
        }
        let gate = self
            .gates
            .lock()
            .unwrap()
            .entry(repo.into())
            .or_default()
            .clone();
        // A per-repository priority gate coalesces identical misses, including
        // across workspaces, without holding the cache mutex through Git I/O.
        let _permit = gate.acquire(speculative)?;
        let mut found = HashMap::new();
        let mut missing = vec![];
        for id in ids {
            if found.contains_key(id) {
                continue;
            }
            if let Some(tree) = self.cached(&(repo.into(), id.clone())) {
                found.insert(id.clone(), tree);
                continue;
            }
            let path = root
                .join("tree-cache/v1/trees")
                .join(repo)
                .join(format!("{id}.json"));
            let disk = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<DiskTree>(&b).ok());
            if let Some(d) = disk.filter(|d| {
                d.format == 1
                    && d.repo == repo
                    && d.oid == *id
                    && valid_entries(&d.entries)
                    && serde_json::to_vec(&d.entries).is_ok_and(|b| hash(&b) == d.checksum)
            }) {
                let tree = Arc::new(d.entries);
                self.remember((repo.into(), id.clone()), tree.clone());
                found.insert(id.clone(), tree);
                self.hits.fetch_add(1, Ordering::Relaxed);
            } else if !missing.contains(id) {
                missing.push(id.clone());
            }
        }
        if !missing.is_empty() {
            activity::stage(&format!(
                "Tree {} batch: {} directories ({repo})",
                if speculative { "lookahead" } else { "demand" },
                missing.len()
            ));
            activity::event(
                "info",
                &format!(
                    "Load {} tree objects: repository {repo}; speculative={speculative}",
                    missing.len()
                ),
            );
            let _lock = crate::util::Lock::take(&root.join("locks").join(repo))?;
            let objects = objects(&root.join("repos").join(repo), &missing, "tree")?;
            for (id, bytes) in missing.iter().zip(objects) {
                activity::check()?;
                let entries = parse_tree(&bytes, id.len() / 2)?;
                let checksum = hash(&serde_json::to_vec(&entries)?);
                let disk = DiskTree {
                    format: 1,
                    repo: repo.into(),
                    oid: id.clone(),
                    checksum,
                    entries: entries.clone(),
                };
                atomic(
                    &root
                        .join("tree-cache/v1/trees")
                        .join(repo)
                        .join(format!("{id}.json")),
                    &serde_json::to_vec(&disk)?,
                )?;
                let tree = Arc::new(entries);
                self.remember((repo.into(), id.clone()), tree.clone());
                found.insert(id.clone(), tree);
                self.loads.fetch_add(1, Ordering::Relaxed);
            }
        }
        activity::check()?;
        Ok(ids.iter().map(|id| found[id].clone()).collect())
    }
}
fn valid_entries(entries: &Tree) -> bool {
    entries.iter().all(|(n, e)| {
        !n.is_empty()
            && !n.contains('/')
            && valid_path(n)
            && oid(&e.oid)
            && matches!(e.mode, 0o40000 | 0o100644 | 0o100755 | 0o120000 | 0o160000)
    })
}
fn parse_tree(mut bytes: &[u8], width: usize) -> Result<Tree> {
    let mut entries = BTreeMap::new();
    while !bytes.is_empty() {
        activity::check()?;
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .ok_or("truncated tree entry")?;
        let header = std::str::from_utf8(&bytes[..end])?;
        let (mode, name) = header.split_once(' ').ok_or("invalid tree entry")?;
        let raw = bytes
            .get(end + 1..end + 1 + width)
            .ok_or("truncated tree object ID")?;
        let id = raw.iter().map(|b| format!("{b:02x}")).collect();
        let entry = TreeEntry {
            mode: u32::from_str_radix(mode, 8)?,
            oid: id,
        };
        if entries.insert(name.into(), entry).is_some() {
            return Err(error("duplicate tree name"));
        }
        bytes = &bytes[end + 1 + width..];
    }
    if !valid_entries(&entries) {
        return Err(error("unsupported Git tree entry or filename"));
    }
    Ok(entries)
}
/// Batch raw immutable objects; the Git helper retains cancellation and diagnostics.
pub(crate) fn objects(repo: &Path, ids: &[String], kind: &str) -> Result<Vec<Vec<u8>>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let input = ids.join("\n") + "\n";
    let bytes = git::input(repo, &["cat-file", "--batch"], Some(input.as_bytes()))?;
    let mut rest = bytes.as_slice();
    let mut result = vec![];
    for id in ids {
        activity::check()?;
        let end = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("truncated batch header")?;
        let fields: Vec<_> = std::str::from_utf8(&rest[..end])?
            .split_whitespace()
            .collect();
        if fields.len() != 3 || fields[0] != id || fields[1] != kind {
            return Err(error(format!("missing or invalid {kind} object {id}")));
        }
        let size: usize = fields[2].parse()?;
        rest = &rest[end + 1..];
        let data = rest.get(..size).ok_or("truncated batch object")?;
        result.push(data.to_vec());
        if rest.get(size) != Some(&b'\n') {
            return Err(error("invalid batch terminator"));
        }
        rest = &rest[size + 1..];
    }
    if !rest.is_empty() {
        return Err(error("extra batch output"));
    }
    Ok(result)
}
