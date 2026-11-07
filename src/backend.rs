use crate::{
    cache::{hash, Lock, Span},
    db::Store,
    error,
    model::*,
    Result,
};
use rusqlite::{params, OptionalExtension};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
pub(crate) type SharedTree = std::sync::Arc<BTreeMap<String, Entry>>;
// Retain at most one parsed tree per repository, rather than every historical tree.

pub static GIT_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub type Progress = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

pub fn git(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run(path, args, None)
}
pub fn run(path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    run_git(path, args, input, None, false)
}
pub(crate) fn run_git(
    path: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    progress: Option<Progress>,
    local_only: bool,
) -> Result<Vec<u8>> {
    crate::metadata::check_io()?;
    GIT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut cmd = Command::new("git");
    cmd.arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if local_only {
        cmd.env("GIT_NO_LAZY_FETCH", "1");
    }
    let timeout = std::env::var("GTL_BACKEND_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let out = execute_command_progress(
        &mut cmd,
        input,
        std::time::Duration::from_secs(timeout),
        progress,
    )?;
    if !out.status.success() {
        return Err(error(format!(
            "git {} failed ({}): {}",
            if args.first() == Some(&"-c") {
                args.get(2).unwrap_or(&"operation")
            } else {
                args.first().unwrap_or(&"operation")
            },
            out.status,
            redact(&String::from_utf8_lossy(&out.stderr))
        )));
    }
    Ok(out.stdout)
}
impl Store {
    pub fn repo_add(&self, id: &str, origin: &str) -> Result<()> {
        if !name_ok(id) || origin.starts_with('-') {
            return Err(error("invalid repository ID/origin"));
        }
        self.conn()?.execute(
            "INSERT INTO repos(id,origin) VALUES(?1,?2)",
            params![id, origin],
        )?;
        Ok(())
    }
    pub fn repo_path(&self, id: &str) -> PathBuf {
        self.root.join("repos").join(id)
    }
    pub fn fetch(&self, id: &str) -> Result<()> {
        self.with_progress_context(&[(id, Vec::new())])?
            .fetch_revisions(id, None)
    }
    fn fetch_revisions(&self, id: &str, revisions: Option<&[String]>) -> Result<()> {
        if !name_ok(id) {
            return Err(error("invalid repository"));
        }
        (self.progress)(&format!("{id}: waiting for repository lock"));
        let span = Span::new(&self.root, &format!("fetch:{id}"));
        let result: Result<()> = (|| {
            let _l = Lock::take(&self.root.join("locks").join(format!("repo-{id}")))?;
            let origin: String =
                self.conn()?
                    .query_row("SELECT origin FROM repos WHERE id=?1", [id], |r| r.get(0))?;
            let p = self.repo_path(id);
            std::fs::create_dir_all(&p)?;
            if !p.join("HEAD").exists() {
                git(&p, &["init", "--bare"])?;
                git(&p, &["remote", "add", "origin", &origin])?;
                git(&p, &["config", "remote.origin.promisor", "true"])?;
                git(
                    &p,
                    &["config", "remote.origin.partialclonefilter", "blob:none"],
                )?;
            }
            let mut specs = vec![];
            if let Some(revisions) = revisions {
                for revision in revisions {
                    if is_oid(revision) {
                        if run_git(
                            &p,
                            &["cat-file", "-e", &format!("{revision}^{{commit}}")],
                            None,
                            None,
                            true,
                        )
                        .is_ok()
                        {
                            (self.progress)(&format!("{id}: cached pin {revision}"));
                            continue;
                        }
                        specs.push(revision.clone());
                    } else if revision.starts_with("refs/tags/") {
                        specs.push(format!("+{revision}:{revision}"));
                    } else {
                        let branch = revision.strip_prefix("refs/heads/").unwrap_or(revision);
                        specs.push(format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"));
                    }
                }
            } else {
                specs.extend([
                    "+refs/heads/*:refs/remotes/origin/*".into(),
                    "+refs/tags/*:refs/tags/*".into(),
                ]);
            }
            specs.sort();
            specs.dedup();
            if !specs.is_empty() {
                (self.progress)(&format!(
                    "{id}: fetching {} requested ref(s), blobs deferred",
                    specs.len()
                ));
                let mut args = vec![
                    "fetch",
                    "--progress",
                    "--filter=blob:none",
                    "--no-tags",
                    "origin",
                ];
                args.extend(specs.iter().map(String::as_str));
                let sink = self.progress.clone();
                let repo = id.to_string();
                run_git(
                    &p,
                    &args,
                    None,
                    Some(std::sync::Arc::new(move |line| {
                        sink(&format!("{repo}: {line}"))
                    })),
                    false,
                )?;
            }
            Ok(())
        })();
        span.event(
            if result.is_ok() {
                "succeeded"
            } else {
                "failed"
            },
            Some(&match &result {
                Ok(()) => "requested refs fetched".to_string(),
                Err(e) => redact(&e.to_string()),
            }),
        );
        match &result {
            Ok(()) => (self.progress)(&format!("{id}: fetch complete")),
            Err(e) => (self.progress)(&format!("{id}: fetch failed: {e}")),
        }
        result.map_err(|e| error(format!("repository {id}: {e}")))
    }
    pub fn resolve_map(&self, map: &Map) -> Result<Map> {
        map.validate()?;
        let mut paths = BTreeMap::<&str, Vec<&str>>::new();
        for mount in &map.mount {
            paths
                .entry(&mount.repo)
                .or_default()
                .push(if mount.path.is_empty() {
                    "/"
                } else {
                    &mount.path
                });
        }
        self.with_progress_context(&paths.into_iter().collect::<Vec<_>>())?
            .resolve_map_inner(map)
    }
    fn with_progress_context(&self, repositories: &[(&str, Vec<&str>)]) -> Result<Self> {
        let mut labels = Vec::new();
        let conn = self.conn()?;
        for (id, paths) in repositories {
            let origin: String =
                conn.query_row("SELECT origin FROM repos WHERE id=?1", [id], |r| r.get(0))?;
            let origin = short_origin(&origin);
            let context = if paths.is_empty() {
                origin
            } else {
                format!("{} · {origin}", paths.join(", "))
            };
            let context: String = context
                .chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .collect();
            labels.push((format!("{id}:"), format!("{id} [{context}]:")));
        }
        let sink = self.progress.clone();
        let mut store = self.clone();
        store.progress = std::sync::Arc::new(move |line| {
            if let Some((prefix, label)) =
                labels.iter().find(|(prefix, _)| line.starts_with(prefix))
            {
                sink(&format!("{label}{}", &line[prefix.len()..]));
            } else {
                sink(line);
            }
        });
        Ok(store)
    }
    fn resolve_map_inner(&self, map: &Map) -> Result<Map> {
        let mut requested = BTreeMap::<String, Vec<String>>::new();
        for mount in &map.mount {
            let revisions = requested.entry(mount.repo.clone()).or_default();
            if !revisions.contains(&mount.revision) {
                revisions.push(mount.revision.clone());
            }
        }
        for id in requested.keys() {
            (self.progress)(&format!("{id}: pending"));
        }
        let requested: Vec<_> = requested.into_iter().collect();
        for chunk in requested.chunks(4) {
            std::thread::scope(|scope| -> Result<()> {
                let jobs: Vec<_> = chunk
                    .iter()
                    .map(|(id, revisions)| {
                        scope.spawn(move || self.fetch_revisions(id, Some(revisions)))
                    })
                    .collect();
                let mut result = Ok(());
                for job in jobs {
                    let outcome = job
                        .join()
                        .map_err(|_| error("fetch worker panicked"))
                        .and_then(|r| r);
                    if result.is_ok() {
                        result = outcome;
                    }
                }
                result
            })?;
        }
        let mut m = map.clone();
        for mount in &mut m.mount {
            let p = self.repo_path(&mount.repo);
            let rev = if let Some(b) = mount.revision.strip_prefix("refs/heads/") {
                format!("refs/remotes/origin/{b}")
            } else if mount.revision.starts_with("refs/tags/")
                || ((mount.revision.len() == 40 || mount.revision.len() == 64)
                    && mount.revision.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                mount.revision.clone()
            } else {
                format!("refs/remotes/origin/{}", mount.revision)
            };
            let oid = String::from_utf8(git(
                &p,
                &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
            )?)?
            .trim()
            .to_string();
            git(&p, &["update-ref", &format!("refs/gtl/bases/{oid}"), &oid])?;
            mount.commit = Some(oid.clone());
            (self.progress)(&format!(
                "{}: indexing tree for mount {}",
                mount.repo, mount.id
            ));
            self.tree(mount)?;
            (self.progress)(&format!("{}: ready ({oid})", mount.repo));
        }
        for child in &m.mount {
            if let Some(parent) = m
                .mount
                .iter()
                .filter(|p| {
                    p.id != child.id
                        && (p.path.is_empty() || child.path.starts_with(&(p.path.clone() + "/")))
                })
                .max_by_key(|p| p.path.len())
            {
                let rel = child
                    .path
                    .strip_prefix(&parent.path)
                    .unwrap()
                    .trim_start_matches('/');
                if let Some(e) = self.tree(parent)?.get(rel) {
                    if e.kind != Kind::Directory {
                        return Err(error("mount shadows a file or symlink"));
                    }
                }
            }
        }
        Ok(m)
    }
    pub fn tree(&self, m: &Mount) -> Result<BTreeMap<String, Entry>> {
        Ok((*self.tree_shared(m)?).clone())
    }
    pub(crate) fn tree_shared(&self, m: &Mount) -> Result<SharedTree> {
        let commit = m.commit.as_ref().ok_or_else(|| error("unresolved mount"))?;
        let spec = if m.subtree.is_empty() {
            commit.clone()
        } else {
            format!("{commit}:{}", m.subtree)
        };
        let key = hash(spec.as_bytes());
        let memory_key = format!("tree:{}:{key}", m.repo);
        if let Some(tree) = self.runtime.get(&memory_key) {
            return Ok(tree);
        }
        if let Some(data) = self
            .conn()?
            .query_row(
                "SELECT data FROM tree_cache WHERE repo=?1 AND tree_key=?2",
                params![m.repo, key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            let mut entries: BTreeMap<String, Entry> = serde_json::from_str(&data)?;
            // Git tree mode 040000 carries no POSIX permission bits. Normalize
            // old persisted caches as well as newly enumerated directory entries.
            for entry in entries.values_mut() {
                if entry.kind == Kind::Directory {
                    entry.mode = 0o755;
                }
            }
            let bytes = data.len() * 3 + 128;
            return self.runtime.put(memory_key, entries, bytes);
        }
        let _lock = Lock::take(
            &self
                .root
                .join("locks")
                .join(format!("tree-{}-{key}", m.repo)),
        )?;
        let out = git(
            &self.repo_path(&m.repo),
            &["ls-tree", "-r", "-t", "-z", &spec],
        )?;
        let mut entries = BTreeMap::new();
        for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .ok_or_else(|| error("invalid tree"))?;
            let meta = std::str::from_utf8(&record[..tab])?;
            let fields: Vec<_> = meta.split_whitespace().collect();
            let path = std::str::from_utf8(&record[tab + 1..])?.to_string();
            let mode = u32::from_str_radix(fields[0], 8)?;
            let kind = match fields[1] {
                "tree" => Kind::Directory,
                "blob" if mode == 0o120000 => Kind::Symlink,
                "blob" => Kind::File,
                "commit" => continue,
                _ => return Err(error("unsupported Git entry")),
            };
            entries.insert(
                path,
                Entry {
                    kind,
                    mode: if fields[1] == "tree" {
                        0o755
                    } else {
                        mode & 0o777
                    },
                    content: format!("git:{}:{}", m.repo, fields[2]),
                    size: 0,
                },
            );
        }
        self.conn()?.execute(
            "INSERT OR REPLACE INTO tree_cache(repo,tree_key,data) VALUES(?1,?2,?3)",
            params![m.repo, key, serde_json::to_string(&entries)?],
        )?;
        let bytes = entries
            .iter()
            .map(|(p, e)| p.len() + e.content.len() + 192)
            .sum();
        self.runtime.put(memory_key, entries, bytes)
    }
    /// Fetch exact missing blobs like Git's promisor-remote implementation.
    /// Normal negotiation can declare a commit common while its promised blobs
    /// are absent locally. Never use that negotiation for object hydration.
    pub(crate) fn ensure_blobs(&self, repo: &str, oids: &[String]) -> Result<Vec<u64>> {
        self.ensure_blobs_with_priority(repo, oids, false)
    }
    pub(crate) fn ensure_blobs_with_priority(
        &self,
        repo: &str,
        oids: &[String],
        speculative: bool,
    ) -> Result<Vec<u64>> {
        if oids.is_empty() {
            return Ok(vec![]);
        }
        let initial = self.blob_sizes_local(repo, oids)?;
        if initial.iter().all(Option::is_some) {
            return Ok(initial.into_iter().map(Option::unwrap).collect());
        }
        let origin: String =
            self.conn()?
                .query_row("SELECT origin FROM repos WHERE id=?1", [repo], |r| r.get(0))?;
        let label: String = format!("{repo} [{}]", short_origin(&origin))
            .chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect();
        let mut activity = self.runtime.activity.begin(
            &self.root,
            &format!("fetch-blobs:{repo}"),
            label,
            &format!("requested {}", oids.join(" ")),
        );
        let result = (|| -> Result<Vec<u64>> {
            let gate = self.runtime.fetch_gate(repo);
            activity.phase(if speculative {
                "waiting for lookahead fetch slot"
            } else {
                "waiting for demand fetch slot"
            });
            let _priority = gate.acquire(speculative);
            // Same lock used by activation, explicit fetch and payload hydration.
            let _lock = Lock::take(&self.root.join("locks").join(format!("repo-{repo}")))?;
            let mut sizes = self.blob_sizes_local(repo, oids)?;
            for attempt in 1..=2 {
                let missing: Vec<_> = oids
                    .iter()
                    .zip(&sizes)
                    .filter(|(_, size)| size.is_none())
                    .map(|(oid, _)| oid.as_str())
                    .collect();
                if missing.is_empty() {
                    return Ok(sizes.into_iter().map(Option::unwrap).collect());
                }
                self.runtime
                    .metrics
                    .metadata_fetches
                    .fetch_add(missing.len() as u64, std::sync::atomic::Ordering::Relaxed);
                activity.phase(&format!(
                    "fetching {} blobs, attempt {attempt}/2",
                    missing.len()
                ));
                let fetch = run_git(
                    &self.repo_path(repo),
                    &[
                        "-c",
                        "fetch.negotiationAlgorithm=noop",
                        "fetch",
                        "--no-tags",
                        "--no-write-fetch-head",
                        "--recurse-submodules=no",
                        "--no-auto-maintenance",
                        "--filter=blob:none",
                        "--progress",
                        "--stdin",
                        "origin",
                    ],
                    Some((missing.join("\n") + "\n").as_bytes()),
                    Some(activity.progress()),
                    false,
                );
                activity.phase("verifying requested blobs locally");
                sizes = self.blob_sizes_local(repo, oids)?;
                if sizes.iter().all(Option::is_some) {
                    if let Err(e) = fetch {
                        activity.event(
                            "recovered",
                            &format!(
                                "fetch reported an error but every requested blob verified: {e}"
                            ),
                        );
                    }
                    return Ok(sizes.into_iter().map(Option::unwrap).collect());
                }
                let failure = fetch
                    .err()
                    .unwrap_or_else(|| error("fetch completed but requested blobs remain missing"));
                activity.event("attempt-failed", &redact(&failure.to_string()));
                if attempt == 2 || !retryable_fetch(&failure.to_string()) {
                    let missing = oids
                        .iter()
                        .zip(&sizes)
                        .filter(|(_, size)| size.is_none())
                        .map(|(oid, _)| oid.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    return Err(error(format!(
                        "repository {repo}; missing blobs {missing}: {failure}"
                    )));
                }
                self.runtime
                    .metrics
                    .fetch_retries
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                activity.phase("retrying transient fetch failure in 250ms");
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            unreachable!()
        })();
        if result.is_err() {
            self.runtime
                .metrics
                .fetch_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if let Ok(sizes) = &result {
            activity.event(
                "verified",
                &format!(
                    "{} blobs; {} object bytes",
                    sizes.len(),
                    sizes.iter().map(|s| *s as u128).sum::<u128>()
                ),
            );
        }
        activity.finish(&result);
        result
    }
    fn blob_sizes_local(&self, repo: &str, oids: &[String]) -> Result<Vec<Option<u64>>> {
        if oids.iter().any(|oid| !is_oid(oid)) {
            return Err(error("invalid blob object ID"));
        }
        let input = oids.join("\n") + "\n";
        let out = run_git(
            &self.repo_path(repo),
            &["cat-file", "--batch-check"],
            Some(input.as_bytes()),
            None,
            true,
        )?;
        parse_blob_sizes(oids, &out)
    }
    pub fn bytes(&self, e: &Entry) -> Result<Vec<u8>> {
        self.runtime
            .metrics
            .payload_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(s) = e.content.strip_prefix("git:") {
            let (repo, oid) = s
                .split_once(':')
                .ok_or_else(|| error("invalid git reference"))?;
            let _l = Lock::take(
                &self
                    .root
                    .join("locks")
                    .join(format!("blob-{}", hash(s.as_bytes()))),
            )?;
            if let Some(id) = self.cached(repo, oid)? {
                if let Ok(b) = self.fetched.get(&id) {
                    self.conn()?.execute(
                        "UPDATE objects SET accessed=unixepoch() WHERE repo=?1 AND oid=?2",
                        params![repo, oid],
                    )?;
                    self.remember_size(e, b.len() as u64)?;
                    return Ok(b);
                }
            }
            let span = Span::new(&self.root, &format!("hydrate:{repo}:{oid}"));
            let result = (|| -> Result<Vec<u8>> {
                self.ensure_blobs(repo, &[oid.to_string()])?;
                let _repo_lock = Lock::take(&self.root.join("locks").join(format!("repo-{repo}")))?;
                let mut b = run_git(
                    &self.repo_path(repo),
                    &["cat-file", "blob", oid],
                    None,
                    None,
                    true,
                )?;
                if b.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
                    let pointer = String::from_utf8(b.clone())?;
                    let expected = pointer
                        .lines()
                        .find_map(|l| l.strip_prefix("oid sha256:"))
                        .ok_or_else(|| error("invalid LFS pointer"))?;
                    let size: u64 = pointer
                        .lines()
                        .find_map(|l| l.strip_prefix("size "))
                        .ok_or_else(|| error("invalid LFS size"))?
                        .parse()?;
                    let mut lfs = self.runtime.activity.begin(
                        &self.root,
                        &format!("lfs:{repo}:{oid}"),
                        format!("{repo} LFS"),
                        &format!("sha256:{expected}; {size} payload bytes"),
                    );
                    lfs.phase("retrieving and verifying LFS payload");
                    let result = (|| -> Result<Vec<u8>> {
                        let mut cmd = Command::new("git");
                        cmd.arg("-C")
                            .arg(self.repo_path(repo))
                            .args(["lfs", "smudge"])
                            .env_remove("GIT_LFS_SKIP_SMUDGE")
                            .env("GIT_TERMINAL_PROMPT", "0")
                            .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
                        let output = execute_command(
                            &mut cmd,
                            Some(&b),
                            std::time::Duration::from_secs(300),
                        )?;
                        if !output.status.success() {
                            return Err(error(format!(
                                "LFS retrieval failed for {repo}:{oid}: {}",
                                redact(&String::from_utf8_lossy(&output.stderr))
                            )));
                        }
                        let bytes = output.stdout;
                        if hash(&bytes) != expected || bytes.len() as u64 != size {
                            return Err(error("LFS integrity mismatch"));
                        }
                        Ok(bytes)
                    })();
                    lfs.finish(&result);
                    b = result?;
                }
                self.remember_size(e, b.len() as u64)?;
                let id = self.fetched.put(&b)?;
                self.conn()?.execute(
                    "INSERT OR REPLACE INTO objects(repo,oid,content,size) VALUES(?1,?2,?3,?4)",
                    params![repo, oid, id, b.len()],
                )?;
                Ok(b)
            })();
            span.event(
                if result.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                },
                Some(&match &result {
                    Ok(bytes) => format!("{} payload bytes", bytes.len()),
                    Err(e) => redact(&e.to_string()),
                }),
            );
            result
        } else {
            self.cache.get(&e.content)
        }
    }
}
/// Drain both pipes concurrently; kill the whole subprocess group on deadline.
pub fn execute_command(
    cmd: &mut Command,
    input: Option<&[u8]>,
    timeout: std::time::Duration,
) -> Result<std::process::Output> {
    execute_command_progress(cmd, input, timeout, None)
}
fn execute_command_progress(
    cmd: &mut Command,
    input: Option<&[u8]>,
    timeout: std::time::Duration,
    progress: Option<Progress>,
) -> Result<std::process::Output> {
    cmd.process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut b = vec![];
        stdout.read_to_end(&mut b).map(|_| b)
    });
    let err = std::thread::spawn(move || {
        let mut b = vec![];
        let mut chunk = [0u8; 4096];
        let mut line = Vec::new();
        loop {
            let n = stderr.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            b.extend_from_slice(&chunk[..n]);
            if let Some(ref progress) = progress {
                for &byte in &chunk[..n] {
                    if byte == b'\r' || byte == b'\n' {
                        if !line.is_empty() {
                            progress(&redact(&String::from_utf8_lossy(&line)));
                            line.clear();
                        }
                    } else {
                        line.push(byte);
                    }
                }
            }
        }
        if let Some(progress) = progress {
            if !line.is_empty() {
                progress(&redact(&String::from_utf8_lossy(&line)));
            }
        }
        Ok::<_, std::io::Error>(b)
    });
    let writer = if let Some(b) = input {
        let b = b.to_vec();
        let mut stdin = child.stdin.take().unwrap();
        Some(std::thread::spawn(move || stdin.write_all(&b)))
    } else {
        None
    };
    let start = std::time::Instant::now();
    let (status, timedout) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if start.elapsed() >= timeout {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            break (child.wait()?, true);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let stdout = out.join().map_err(|_| error("stdout worker failed"))??;
    let stderr = err.join().map_err(|_| error("stderr worker failed"))??;
    if let Some(w) = writer {
        let r = w.join().map_err(|_| error("stdin worker failed"))?;
        if status.success() {
            r?;
        }
    }
    if timedout {
        return Err(error(format!(
            "backend command timed out after {} seconds",
            timeout.as_secs_f64()
        )));
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}
pub fn redact(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            if word.contains("://") || word.contains("token=") || word.contains("password=") {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_oid(revision: &str) -> bool {
    matches!(revision.len(), 40 | 64) && revision.bytes().all(|b| b.is_ascii_hexdigit())
}

// Show repository identity without URL userinfo, query parameters or fragments.
fn short_origin(origin: &str) -> String {
    let clean = origin.split(['?', '#']).next().unwrap_or("");
    let path = if let Some((scheme, rest)) = clean.split_once("://") {
        if scheme == "file" {
            rest.rsplit('/').find(|s| !s.is_empty()).unwrap_or("local")
        } else {
            // Discard the authority entirely, including credentials and port.
            rest.split_once('/')
                .map(|(_, path)| path)
                .unwrap_or("remote")
        }
    } else if !clean.starts_with('/')
        && !clean.starts_with("./")
        && !clean.starts_with("../")
        && clean.contains(':')
    {
        clean.split_once(':').map(|(_, path)| path).unwrap_or(clean)
    } else {
        clean.rsplit('/').find(|s| !s.is_empty()).unwrap_or("local")
    };
    let path = path.trim_matches('/');
    path.strip_suffix(".git").unwrap_or(path).to_string()
}

#[cfg(test)]
mod progress_label_tests {
    use super::short_origin;
    #[test]
    fn origins_are_short_and_omit_credentials() {
        for origin in [
            "https://github.com/ROCm/llvm-project.git",
            "https://user:secret@github.com/ROCm/llvm-project.git?token=secret#private",
            "ssh://git@github.com:22/ROCm/llvm-project.git",
            "git@github.com:ROCm/llvm-project.git",
        ] {
            assert_eq!(short_origin(origin), "ROCm/llvm-project");
        }
        assert_eq!(short_origin("/tmp/local.git"), "local");
        assert_eq!(short_origin("file:///tmp/local.git"), "local");
        assert_eq!(short_origin("https://user:secret@host"), "remote");
    }
}

fn parse_blob_sizes(oids: &[String], output: &[u8]) -> Result<Vec<Option<u64>>> {
    let text = std::str::from_utf8(output)?;
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != oids.len() {
        return Err(error("truncated or extra blob size responses"));
    }
    oids.iter()
        .zip(lines)
        .map(|(oid, line)| {
            let fields: Vec<_> = line.split_whitespace().collect();
            match fields.as_slice() {
                [id, "missing"] if *id == oid => Ok(None),
                [id, "blob", size] if *id == oid => Ok(Some(size.parse()?)),
                _ => Err(error(
                    "invalid blob size response (object ID or type mismatch)",
                )),
            }
        })
        .collect()
}
fn retryable_fetch(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "connection reset",
        "connection refused",
        "could not resolve host",
        "temporary failure",
        "early eof",
        "remote end hung up",
        "http 500",
        "http 502",
        "http 503",
        "http 504",
        "error: 500",
        "error: 502",
        "error: 503",
        "error: 504",
        "did not send all necessary objects",
        "requested blobs remain missing",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod blob_response_tests {
    use super::*;
    #[test]
    fn rejects_incomplete_mismatched_or_nonblob_responses() {
        let oid = "a".repeat(40);
        let oids = vec![oid.clone()];
        assert!(parse_blob_sizes(&oids, b"").is_err());
        assert!(
            parse_blob_sizes(&oids, format!("{oid} blob 4\n{oid} blob 4\n").as_bytes()).is_err()
        );
        assert!(
            parse_blob_sizes(&oids, format!("{} blob 4\n", "b".repeat(40)).as_bytes()).is_err()
        );
        assert!(parse_blob_sizes(&oids, format!("{oid} tree 4\n").as_bytes()).is_err());
        assert_eq!(
            parse_blob_sizes(&oids, format!("{oid} missing\n").as_bytes()).unwrap(),
            vec![None]
        );
        assert_eq!(
            parse_blob_sizes(&oids, format!("{oid} blob 0\n").as_bytes()).unwrap(),
            vec![Some(0)]
        );
    }
    #[test]
    fn retries_transient_failures_but_not_auth_or_missing_remote_objects() {
        for e in [
            "connection reset",
            "HTTP 503",
            "remote did not send all necessary objects",
            "timed out",
        ] {
            assert!(retryable_fetch(e));
        }
        for e in [
            "Authentication failed",
            "not our ref deadbeef",
            "Permission denied",
            "repository does not exist",
        ] {
            assert!(!retryable_fetch(e));
        }
    }
}
