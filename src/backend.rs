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
pub fn git(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run(path, args, None)
}
pub fn run(path: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
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
    let timeout = std::env::var("GTL_BACKEND_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let out = execute_command(&mut cmd, input, std::time::Duration::from_secs(timeout))?;
    if !out.status.success() {
        return Err(error(format!(
            "git {} failed ({}): {}",
            args.first().unwrap_or(&"operation"),
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
        if !name_ok(id) {
            return Err(error("invalid repository"));
        }
        let span = Span::new(&self.root, &format!("fetch:{id}"));
        let result = (|| {
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
            git(
                &p,
                &[
                    "fetch",
                    "--filter=blob:none",
                    "origin",
                    "+refs/heads/*:refs/remotes/origin/*",
                    "+refs/tags/*:refs/tags/*",
                ],
            )?;
            Ok(())
        })();
        span.event(
            if result.is_ok() {
                "succeeded"
            } else {
                "failed"
            },
            Some("fetch completed; errors are returned to caller"),
        );
        result
    }
    pub fn resolve_map(&self, map: &Map) -> Result<Map> {
        map.validate()?;
        let ids: Vec<_> = map
            .mount
            .iter()
            .map(|m| m.repo.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for chunk in ids.chunks(4) {
            std::thread::scope(|scope| -> Result<()> {
                let jobs: Vec<_> = chunk
                    .iter()
                    .map(|id| scope.spawn(move || self.fetch(id)))
                    .collect();
                for j in jobs {
                    j.join().map_err(|_| error("fetch worker panicked"))??;
                }
                Ok(())
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
            mount.commit = Some(oid);
            self.tree(mount)?;
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
        let commit = m.commit.as_ref().ok_or_else(|| error("unresolved mount"))?;
        let spec = if m.subtree.is_empty() {
            commit.clone()
        } else {
            format!("{commit}:{}", m.subtree)
        };
        let key = hash(spec.as_bytes());
        if let Some(data) = self
            .conn()?
            .query_row(
                "SELECT data FROM tree_cache WHERE repo=?1 AND tree_key=?2",
                params![m.repo, key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(serde_json::from_str(&data)?);
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
                    mode: mode & 0o777,
                    content: format!("git:{}:{}", m.repo, fields[2]),
                    size: 0,
                },
            );
        }
        self.conn()?.execute(
            "INSERT OR REPLACE INTO tree_cache(repo,tree_key,data) VALUES(?1,?2,?3)",
            params![m.repo, key, serde_json::to_string(&entries)?],
        )?;
        Ok(entries)
    }
    pub fn bytes(&self, e: &Entry) -> Result<Vec<u8>> {
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
                    return Ok(b);
                }
            }
            let span = Span::new(&self.root, &format!("hydrate:{repo}:{oid}"));
            let result = (|| -> Result<Vec<u8>> {
                let _repo_lock = Lock::take(&self.root.join("locks").join(format!("repo-{repo}")))?;
                let mut b = git(&self.repo_path(repo), &["cat-file", "blob", oid])?;
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
                    let mut cmd = Command::new("git");
                    cmd.arg("-C")
                        .arg(self.repo_path(repo))
                        .args(["lfs", "smudge"])
                        .env_remove("GIT_LFS_SKIP_SMUDGE")
                        .env("GIT_TERMINAL_PROMPT", "0")
                        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
                    let output =
                        execute_command(&mut cmd, Some(&b), std::time::Duration::from_secs(300))?;
                    if !output.status.success() {
                        return Err(error(format!(
                            "LFS retrieval failed for {repo}:{oid}: {}",
                            redact(&String::from_utf8_lossy(&output.stderr))
                        )));
                    }
                    b = output.stdout;
                    if hash(&b) != expected || b.len() as u64 != size {
                        return Err(error("LFS integrity mismatch"));
                    }
                }
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
                Some("hydration completed; errors are returned to caller"),
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
        stderr.read_to_end(&mut b).map(|_| b)
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
