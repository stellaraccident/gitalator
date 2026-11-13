//! Dynamic, bounded submodule discovery. Only the coordinator changes the map.
use crate::{
    activity,
    cli::Submodules,
    error, git,
    model::*,
    mounts::{resolve_url, ImportOptions},
    store::Store,
    util::{atomic, hash, Lock},
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

static RUNNING: Mutex<(usize, BTreeSet<PathBuf>)> = Mutex::new((0, BTreeSet::new()));
struct Permit(PathBuf);
impl Permit {
    fn take(limit: usize, repo: PathBuf) -> Option<Self> {
        let mut running = RUNNING.lock().unwrap();
        if running.0 >= limit || running.1.contains(&repo) {
            return None;
        }
        running.0 += 1;
        running.1.insert(repo.clone());
        Some(Self(repo))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut running = RUNNING.lock().unwrap();
        running.0 -= 1;
        running.1.remove(&self.0);
    }
}
fn jobs() -> Result<usize> {
    match std::env::var("GTL_SUBMODULE_JOBS") {
        Ok(s) => s
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=128).contains(n))
            .ok_or_else(|| error("GTL_SUBMODULE_JOBS must be 1..128")),
        Err(std::env::VarError::NotPresent) => Ok(32),
        Err(e) => Err(e.into()),
    }
}
#[derive(Clone)]
struct Work {
    url: String,
    pin: String,
    path: String,
    subtree: String,
    mode: Submodules,
    ancestors: BTreeSet<(String, String)>,
    parent: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Link {
    path: String,
    url: String,
    pin: String,
}
#[derive(Serialize, Deserialize)]
struct Inventory {
    version: u32,
    repo: String,
    commit: String,
    subtree: String,
    checksum: String,
    links: Vec<Link>,
}
#[derive(Clone)]
struct Prepared {
    pin: String,
    links: Vec<Link>,
    cached: bool,
}

fn remote_denied(f: &git::CommandFailure) -> bool {
    let s = f.message.to_ascii_lowercase();
    [
        "host key verification failed",
        "remote host identification has changed",
        "permission denied (",
        "authentication failed",
        "repository not found",
        "repository does not exist",
        "access denied",
        "does not appear to be a git repository",
    ]
    .iter()
    .any(|p| s.contains(p))
}
fn retry(store: &Store, w: &Work, mut f: git::CommandFailure) -> git::CommandFailure {
    f.retry_command = Some(format!("git -C {} fetch --filter=blob:none --recurse-submodules=no --no-tags --no-auto-maintenance origin {}", git::shell_quote(&store.root.join("repos").join(hash(w.url.as_bytes())).to_string_lossy()), git::shell_quote(&w.pin)));
    f
}
impl Store {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn import_forest(
        &self,
        map: &mut Map,
        url: &str,
        pin: &str,
        path: &str,
        subtree: &str,
        mode: Submodules,
        reuse: bool,
        options: &mut ImportOptions,
    ) -> Result<()> {
        let limit = jobs()?;
        let original = map.mounts.len();
        let mut queue = VecDeque::from([Work {
            url: url.into(),
            pin: pin.into(),
            path: path.into(),
            subtree: subtree.into(),
            mode,
            ancestors: BTreeSet::new(),
            parent: None,
        }]);
        let mut busy = BTreeSet::new();
        let mut failures: BTreeMap<(String, String), git::CommandFailure> = BTreeMap::new();
        let mut denied: BTreeMap<String, git::CommandFailure> = BTreeMap::new();
        let mut prepared = BTreeMap::new();
        let mut affected: BTreeMap<String, usize> = BTreeMap::new();
        let stop = Arc::new(AtomicBool::new(false));
        let scope = activity::capture().with_stop(stop.clone());
        let (tx, rx) = mpsc::channel();
        let mut active = 0;
        let mut completed = 0;
        let mut hits = 0;
        let mut progress = Instant::now();
        let mut planned: BTreeSet<_> = map.mounts.iter().map(|m| m.path.clone()).collect();
        planned.insert(path.to_string());
        let result = std::thread::scope(|threads| -> Result<()> {
            let result = (|| -> Result<()> {
                while !queue.is_empty() || active > 0 {
                    activity::check()?;
                    if progress.elapsed() >= Duration::from_secs(1) {
                        let message = format!("Submodules: {} discovered, {completed} completed, {active} active, {} queued, {} skipped, {hits} cache hits, {} failed remotes", completed+active+queue.len(), queue.len(), options.skipped.len(), denied.len());
                        activity::stage(&message);
                        activity::event("info", &message);
                        progress = Instant::now();
                    }
                    // Never fill workers with tasks waiting behind another pin of the same remote.
                    while active < limit {
                        let Some((index, permit)) = queue
                            .iter()
                            .enumerate()
                            .filter(|(_, w)| !busy.contains(&w.url))
                            .find_map(|(i, w)| {
                                Permit::take(
                                    limit,
                                    self.root.join("repos").join(hash(w.url.as_bytes())),
                                )
                                .map(|p| (i, p))
                            })
                        else {
                            break;
                        };
                        let w = queue.remove(index).unwrap();
                        validate_url(&w.url)?;
                        if w.ancestors.len() > 32 {
                            return Err(error("submodule import exceeds depth/mount limit"));
                        }
                        if let Some(m) = map.mounts.iter().find(|m| m.path == w.path) {
                            if !reuse
                                || map.repositories[&m.repository].url != w.url
                                || m.commit != w.pin
                                || m.subtree != w.subtree
                            {
                                return Err(error(format!("submodule mount conflict at {}: existing origin/pin/subtree differs or path is occupied; use an explicit resolve workflow (no mounts changed)", w.path)));
                            }
                        }
                        let key = (
                            w.url.clone(),
                            w.pin.clone(),
                            w.subtree.clone(),
                            w.mode != Submodules::None,
                        );
                        let cached = prepared.get(&key).cloned();
                        let failure = failures.get(&(w.url.clone(), w.pin.clone())).cloned();
                        let circuit = denied.get(&w.url).cloned();
                        busy.insert(w.url.clone());
                        active += 1;
                        let tx = tx.clone();
                        let scope = scope.clone();
                        threads.spawn(move || {
                            let _permit = permit;
                            let _context = scope.enter();
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    if let Some(p) = cached {
                                        return Ok(Prepared { cached: true, ..p });
                                    }
                                    self.prepare_import(&w, circuit, failure)
                                }))
                                .unwrap_or_else(|_| {
                                    Err(error("submodule discovery worker panicked"))
                                });
                            drop(_permit);
                            let _ = tx.send((w, result));
                        });
                    }
                    let (w, result) = match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(r) => r,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(e) => return Err(e.into()),
                    };
                    active -= 1;
                    busy.remove(&w.url);
                    completed += 1;
                    let p = match result {
                        Ok(p) => p,
                        Err(e) => {
                            activity::check()?;
                            let Some(f) = e.downcast_ref::<git::CommandFailure>().filter(|f| {
                                f.command == "fetch"
                                    && options.skip_unavailable
                                    && w.parent.is_some()
                            }) else {
                                return Err(e);
                            };
                            failures.insert((w.url.clone(), w.pin.clone()), f.clone());
                            if remote_denied(f) {
                                denied.insert(w.url.clone(), f.clone());
                            }
                            let count = affected.entry(w.url.clone()).or_default();
                            *count += 1;
                            if *count == 1 {
                                activity::event(
                                    "warn",
                                    &format!("Skipped submodule {}: {f}", w.path),
                                );
                            }
                            if !map.mounts.iter().any(|m| m.path == w.path) {
                                if let Some(parent) = map
                                    .mounts
                                    .iter_mut()
                                    .find(|m| Some(&m.path) == w.parent.as_ref())
                                {
                                    parent.uninitialized_submodules = true;
                                }
                            }
                            options.skipped.push(json!({"path":w.path,"url":git::public_origin(&w.url),"commit":w.pin,"error":format!("git {}: {}", f.command,f.message),"retry_command":f.retry_command}));
                            continue;
                        }
                    };
                    hits += usize::from(p.cached);
                    prepared.insert(
                        (
                            w.url.clone(),
                            w.pin.clone(),
                            w.subtree.clone(),
                            w.mode != Submodules::None,
                        ),
                        p.clone(),
                    );
                    let mut ancestors = w.ancestors;
                    if w.mode != Submodules::None
                        && !ancestors.insert((w.url.clone(), p.pin.clone()))
                    {
                        return Err(error("recursive submodule cycle"));
                    }
                    if let Some(m) = map.mounts.iter_mut().find(|m| m.path == w.path) {
                        if w.mode != Submodules::None {
                            m.uninitialized_submodules = false;
                        }
                    } else {
                        if map.mounts.len() >= 4096 {
                            return Err(error("submodule import exceeds depth/mount limit"));
                        }
                        let repository = map
                            .repositories
                            .iter()
                            .find(|(_, r)| r.url == w.url)
                            .map(|(k, _)| k.clone())
                            .unwrap_or_else(|| format!("repo-{}", &hash(w.url.as_bytes())[..16]));
                        if map
                            .repositories
                            .get(&repository)
                            .is_some_and(|r| r.url != w.url)
                        {
                            return Err(error("repository ID collision"));
                        }
                        map.repositories
                            .insert(repository.clone(), Repository { url: w.url.clone() });
                        map.mounts.push(Mount {
                            id: if w.path.is_empty() {
                                "root".into()
                            } else {
                                format!("mount-{}", &hash(w.path.as_bytes())[..16])
                            },
                            repository,
                            commit: p.pin,
                            path: w.path.clone(),
                            subtree: w.subtree,
                            uninitialized_submodules: w.mode == Submodules::None,
                        });
                    }
                    for link in p.links {
                        planned.insert(join(&w.path, &link.path));
                        if planned.len() > 4096 {
                            return Err(error("submodule import exceeds depth/mount limit"));
                        }
                        queue.push_back(Work {
                            url: resolve_url(&w.url, &link.url)?,
                            pin: link.pin,
                            path: join(&w.path, &link.path),
                            subtree: String::new(),
                            mode: if w.mode == Submodules::Recursive {
                                Submodules::Recursive
                            } else {
                                Submodules::None
                            },
                            ancestors: ancestors.clone(),
                            parent: Some(w.path.clone()),
                        });
                    }
                }
                Ok(())
            })();
            if result.is_err() {
                stop.store(true, Ordering::Release);
            }
            result
        });
        result?;
        map.mounts[original..].sort_by(|a, b| a.path.cmp(&b.path));
        options
            .skipped
            .sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        for (url, count) in affected {
            activity::event(
                "warn",
                &format!(
                    "Skipped {count} submodule paths from {} (retry on next import)",
                    git::public_origin(&url)
                ),
            );
        }
        activity::event(
            "info",
            &format!(
                "Submodules complete: {completed} processed, {hits} cache hits, {} skipped",
                options.skipped.len()
            ),
        );
        map.validate()
    }

    fn prepare_import(
        &self,
        w: &Work,
        circuit: Option<git::CommandFailure>,
        failure: Option<git::CommandFailure>,
    ) -> Result<Prepared> {
        let repo = hash(w.url.as_bytes());
        let bare = self.root.join("repos").join(&repo);
        let cache = self
            .root
            .join("submodule-cache/v1")
            .join(&repo)
            .join(format!(
                "{}.json",
                hash(format!("{}:{}", w.pin, w.subtree).as_bytes())
            ));
        if oid(&w.pin) && bare.join("HEAD").exists() {
            if let Some(i) = fs::read(&cache)
                .ok()
                .and_then(|b| serde_json::from_slice::<Inventory>(&b).ok())
                .filter(|i| {
                    i.version == 1
                        && i.repo == repo
                        && i.commit == w.pin
                        && i.subtree == w.subtree
                        && serde_json::to_vec(&i.links).is_ok_and(|b| hash(&b) == i.checksum)
                })
            {
                return Ok(Prepared {
                    pin: w.pin.clone(),
                    links: if w.mode == Submodules::None {
                        vec![]
                    } else {
                        i.links
                    },
                    cached: true,
                });
            }
        }
        if let Some(f) = failure {
            return Err(Box::new(retry(self, w, f)));
        }
        if let Some(f) = &circuit {
            // A denied remote must not prevent use of an already fetched commit.
            activity::check()?;
            if !oid(&w.pin)
                || git::local(
                    &bare,
                    &["cat-file", "-e", &format!("{}^{{commit}}", w.pin)],
                    None,
                )
                .is_err()
            {
                activity::check()?;
                return Err(Box::new(retry(self, w, f.clone())));
            }
        } else if !bare.join("HEAD").exists() {
            self.repo(&w.url)?;
        }
        let pin = self.fetch(&repo, &w.pin)?;
        if w.mode == Submodules::None {
            return Ok(Prepared {
                pin,
                links: vec![],
                cached: false,
            });
        }
        let commits = self.prepare_commits(&w.url, &BTreeSet::from([pin.clone()]))?;
        let root = &commits[0].tree;
        let entries = self.trees.get(&self.root, &repo, root)?;
        let mut links = vec![];
        if let Some(config) = entries.get(".gitmodules") {
            if config.mode != 0o100644 && config.mode != 0o100755 {
                return Err(error(".gitmodules is not a regular file"));
            }
            // Explicit fetch keeps remote failures distinguishable from malformed configuration.
            if git::local(&bare, &["cat-file", "-e", &config.oid], None).is_err() {
                activity::check()?;
                if let Some(f) = circuit {
                    return Err(Box::new(retry(self, w, f)));
                }
                let _lock = Lock::take(&self.root.join("locks").join(&repo))?;
                git::run(
                    &bare,
                    &[
                        "fetch",
                        "--no-tags",
                        "--no-auto-maintenance",
                        "--recurse-submodules=no",
                        "origin",
                        &config.oid,
                    ],
                )?;
            }
            let bytes = git::run(
                &bare,
                &[
                    "config",
                    "--no-includes",
                    "--null",
                    "--blob",
                    &config.oid,
                    "--list",
                ],
            )?;
            let mut declarations: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
            for pair in bytes.split(|b| *b == 0).filter(|b| !b.is_empty()) {
                let pair = std::str::from_utf8(pair)?;
                let (key, value) = pair.split_once('\n').unwrap_or((pair, ""));
                if let Some(key) = key.strip_prefix("submodule.") {
                    if let Some((name, field @ ("path" | "url"))) = key.rsplit_once('.') {
                        if declarations
                            .entry(name.into())
                            .or_default()
                            .insert(field.into(), value.into())
                            .is_some()
                        {
                            return Err(error("duplicate submodule config key"));
                        }
                    }
                }
            }
            let mut seen = BTreeSet::new();
            let mut pending = vec![];
            for declaration in declarations.values() {
                let path = declaration.get("path").ok_or("submodule missing path")?;
                if path.is_empty() || !valid_path(path) || !seen.insert(path) {
                    return Err(error("invalid or duplicate submodule path"));
                }
                let url = declaration.get("url").ok_or("submodule missing URL")?;
                validate_url(&resolve_url(&w.url, url)?)?;
                if !under(path, &w.subtree) || path == &w.subtree {
                    continue;
                }
                pending.push((path.clone(), url.clone(), root.clone(), 0));
            }
            // Probe only declared paths; fetch each known directory frontier in batches.
            while !pending.is_empty() {
                let ids: Vec<_> = pending
                    .iter()
                    .map(|(_, _, tree, _)| tree.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let mut loaded = BTreeMap::new();
                for chunk in ids.chunks(128) {
                    for (id, tree) in chunk
                        .iter()
                        .zip(self.trees.batch(&self.root, &repo, chunk, false)?)
                    {
                        loaded.insert(id.clone(), tree);
                    }
                }
                let mut next = vec![];
                for (path, url, tree, n) in pending {
                    let parts: Vec<_> = path.split('/').collect();
                    if let Some(e) = loaded[&tree].get(parts[n]) {
                        if n + 1 < parts.len() {
                            if e.mode == 0o40000 {
                                next.push((path, url, e.oid.clone(), n + 1));
                            }
                        } else if e.mode == 0o160000 {
                            links.push(Link {
                                path: if w.subtree.is_empty() {
                                    path
                                } else {
                                    path[w.subtree.len() + 1..].into()
                                },
                                url,
                                pin: e.oid.clone(),
                            });
                        }
                    }
                }
                pending = next;
            }
        }

        links.sort_by(|a, b| a.path.cmp(&b.path));
        let inventory = Inventory {
            version: 1,
            repo,
            commit: pin.clone(),
            subtree: w.subtree.clone(),
            checksum: hash(&serde_json::to_vec(&links)?),
            links: links.clone(),
        };
        let cache = cache.parent().unwrap().join(format!(
            "{}.json",
            hash(format!("{pin}:{}", w.subtree).as_bytes())
        ));
        atomic(&cache, &serde_json::to_vec(&inventory)?)?;
        Ok(Prepared {
            pin,
            links,
            cached: false,
        })
    }
}
fn validate_url(url: &str) -> Result<()> {
    Map {
        format: 1,
        repositories: BTreeMap::from([("repo".into(), Repository { url: url.into() })]),
        mounts: vec![],
    }
    .validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_budget_and_repository_exclusion() {
        let first = Permit::take(2, PathBuf::from("budget-test-one")).unwrap();
        assert!(Permit::take(2, PathBuf::from("budget-test-one")).is_none());
        let second = Permit::take(2, PathBuf::from("budget-test-two")).unwrap();
        assert!(Permit::take(2, PathBuf::from("budget-test-three")).is_none());
        drop(first);
        assert!(Permit::take(2, PathBuf::from("budget-test-three")).is_some());
        drop(second);
    }
    #[test]
    fn only_definitive_remote_failures_open_circuit() {
        for (message, expected) in [
            ("Host key verification failed.", true),
            ("Permission denied (publickey).", true),
            ("fatal: Authentication failed for remote", true),
            ("ERROR: Repository not found.", true),
            ("fatal: remote error: upload-pack: not our ref abc", false),
            ("fatal: couldn't find remote ref abc", false),
            ("HTTP 404", false),
            ("Connection reset by peer", false),
        ] {
            assert_eq!(
                remote_denied(&git::CommandFailure {
                    command: "fetch".into(),
                    message: message.into(),
                    retry_command: None
                }),
                expected,
                "{message}"
            );
        }
    }
}
