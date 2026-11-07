use crate::{backend::git, cache::Span, db::Store, error, model::*, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
};
#[derive(Parser)]
#[command(
    name = "gtl",
    version,
    about = "Versioned workspaces across independent Git repositories"
)]
pub struct Cli {
    #[arg(long, global = true, default_value = ".gtl")]
    pub state_dir: PathBuf,
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Cmd,
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum Cmd {
    Init,
    Config {
        #[arg(long)]
        idle_seconds: Option<u64>,
        #[arg(long)]
        max_seconds: Option<u64>,
        #[arg(long)]
        cache_bytes: Option<u64>,
        #[arg(long)]
        metadata_cache_bytes: Option<u64>,
    },
    Doctor,
    Metrics,
    Warmup {
        workspace: String,
        paths: Vec<String>,
        #[arg(long)]
        recursive: bool,
    },
    Repo {
        #[command(subcommand)]
        command: RepoCmd,
    },
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCmd,
    },
    Mount {
        path: PathBuf,
    },
    Unmount {
        path: PathBuf,
    },
    Status {
        workspace: String,
    },
    Diff {
        workspace: String,
    },
    Map {
        #[command(subcommand)]
        command: MapCmd,
    },
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCmd,
    },
    Restore {
        workspace: String,
        number: i64,
    },
    Stash {
        workspace: String,
    },
    Activate {
        workspace: String,
        #[arg(long)]
        plan: bool,
        #[arg(long = "continue", conflicts_with = "abort")]
        resume: bool,
        #[arg(long)]
        abort: bool,
    },
    Conflicts {
        transaction: String,
    },
    Resolve {
        transaction: String,
        mount: String,
        path: String,
        #[arg(long)]
        discard: bool,
    },
    Export {
        workspace: String,
        destination: PathBuf,
    },
    Import {
        workspace: String,
        export: PathBuf,
    },
    Layout {
        #[command(subcommand)]
        command: LayoutCmd,
    },
    Bundle {
        export: PathBuf,
        destination: PathBuf,
    },
    Review {
        export: PathBuf,
        #[arg(long)]
        publish: bool,
        #[arg(long)]
        base: Option<String>,
    },
    Land {
        export: PathBuf,
        #[arg(long)]
        verify_only: bool,
    },
    Gc {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value_t = 1073741824)]
        max_bytes: u64,
    },
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum RepoCmd {
    Add { id: String, origin: String },
    List,
    Fetch { id: String },
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum WorkspaceCmd {
    Create {
        name: String,
        #[arg(long)]
        map: Option<PathBuf>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long, requires = "from")]
        snapshot: Option<i64>,
    },
    List,
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum MapCmd {
    Show {
        workspace: String,
        #[arg(long)]
        active: bool,
    },
    Set {
        workspace: String,
        file: PathBuf,
    },
    ImportSuperrepo {
        workspace: String,
        path: PathBuf,
        #[arg(long, default_value = "HEAD")]
        revision: String,
    },
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum SnapshotCmd {
    Create { workspace: String },
    List { workspace: String },
    Pin { workspace: String, number: i64 },
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum LayoutCmd {
    Branch { workspace: String, name: String },
    Switch { workspace: String, name: String },
}
pub fn dispatch(s: &Store, cmd: Cmd) -> Result<Value> {
    let span = Span::new(&s.root, cmd.label());
    let result = execute(s, cmd);
    s.runtime.flush()?;
    span.event(
        if result.is_ok() {
            "succeeded"
        } else {
            "failed"
        },
        Some("command completed; errors are returned to client"),
    );
    result
}
impl Cmd {
    fn label(&self) -> &str {
        match self {
            Self::Repo { .. } => "repo",
            Self::Activate { .. } => "activate",
            Self::Export { .. } => "export",
            Self::Import { .. } => "import",
            Self::Snapshot { .. } => "snapshot",
            Self::Gc { .. } => "gc",
            _ => "command",
        }
    }
}
fn execute(s: &Store, cmd: Cmd) -> Result<Value> {
    match cmd {
        Cmd::Init => Ok(json!({"state_dir": s.root, "schema": 2})),
        Cmd::Config {
            idle_seconds,
            max_seconds,
            cache_bytes,
            metadata_cache_bytes,
        } => {
            let idle = idle_seconds.unwrap_or(s.setting("snapshot_idle_seconds", 2)? as u64);
            let max = max_seconds.unwrap_or(s.setting("snapshot_max_seconds", 30)? as u64);
            let cache = cache_bytes.unwrap_or(s.setting("cache_bytes", 1073741824)? as u64);
            let metadata_cache = metadata_cache_bytes
                .unwrap_or(s.setting("metadata_cache_bytes", 512 * 1024 * 1024)? as u64);
            if metadata_cache == 0 || metadata_cache > i64::MAX as u64 {
                return Err(error("metadata cache must be positive and fit i64"));
            }
            if idle == 0 || max < idle || max > i64::MAX as u64 || cache > i64::MAX as u64 {
                return Err(error(
                    "require 0 < idle <= max <= i64::MAX; cache must fit i64",
                ));
            }
            let mut conn = s.conn()?;
            let tx = conn.transaction()?;
            for (key, value) in [
                ("snapshot_idle_seconds", idle),
                ("snapshot_max_seconds", max),
                ("cache_bytes", cache),
                ("metadata_cache_bytes", metadata_cache),
            ] {
                tx.execute(
                    "INSERT OR REPLACE INTO settings(key,value) VALUES(?1,?2)",
                    rusqlite::params![key, value],
                )?;
            }
            tx.commit()?;
            s.runtime.limit(metadata_cache as usize);
            Ok(
                json!({"idle_seconds": idle, "max_seconds": max, "cache_bytes": cache, "metadata_cache_bytes": metadata_cache}),
            )
        }
        Cmd::Metrics => Ok(s.metrics()),
        Cmd::Warmup {
            workspace,
            paths,
            recursive,
        } => s.warmup(&workspace, &paths, recursive),
        Cmd::Doctor => {
            let integrity: String = s
                .conn()?
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            let git_version = String::from_utf8(git(&s.root, &["--version"])?)?;
            Ok(
                json!({"database": integrity, "git": git_version, "fuse_device": Path::new("/dev/fuse").exists()}),
            )
        }
        Cmd::Repo { command } => match command {
            RepoCmd::Add { id, origin } => {
                let origin = if Path::new(&origin).exists() {
                    std::fs::canonicalize(&origin)?
                        .to_string_lossy()
                        .into_owned()
                } else {
                    origin
                };
                s.repo_add(&id, &origin)?;
                Ok(json!({"registered": id}))
            }
            RepoCmd::Fetch { id } => {
                s.fetch(&id)?;
                Ok(json!({"fetched": id}))
            }
            RepoCmd::List => {
                let c = s.conn()?;
                let mut q = c.prepare("SELECT id FROM repos ORDER BY id")?;
                let ids = q
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(json!(ids))
            }
        },
        Cmd::Workspace { command } => match command {
            WorkspaceCmd::List => Ok(json!(s.names()?)),
            WorkspaceCmd::Create {
                name,
                map,
                from,
                snapshot,
            } => {
                if let Some(source) = from {
                    let (revision, draft) = if let Some(number) = snapshot {
                        s.snapshot_revision(&source, number)?
                    } else {
                        (s.head(&source)?, s.draft(&source)?)
                    };
                    s.create(&name, &draft)?;
                    s.conn()?.execute(
                        "UPDATE workspaces SET head=?2 WHERE name=?1",
                        rusqlite::params![name, revision],
                    )?;
                } else {
                    let draft = if let Some(path) = map {
                        std::fs::read_to_string(path)?
                    } else {
                        "format = 1\n".into()
                    };
                    s.create(&name, &draft)?;
                }
                Ok(json!({"workspace": name}))
            }
        },
        Cmd::Status { workspace } => {
            let (mut map, overlay) = s.state(s.head(&workspace)?)?;
            let draft = Map::parse(&s.draft(&workspace)?);
            for mount in &mut map.mount {
                mount.commit = None;
            }
            let valid = draft.is_ok();
            let differs = draft
                .map(|m| serde_json::to_string(&m).ok() != serde_json::to_string(&map).ok())
                .unwrap_or(true);
            Ok(
                json!({"workspace": workspace, "head": s.head(&workspace)?, "frozen": s.frozen(&workspace)?, "overlay_entries": overlay.len(), "draft_valid": valid, "draft_differs": differs}),
            )
        }
        Cmd::Diff { workspace } => {
            let (map, overlay) = s.state(s.head(&workspace)?)?;
            let rows: Vec<_> = overlay.iter().map(|((id,p),entry)| {
                let path = map.mount.iter().find(|m| &m.id == id).map(|m| join(&m.path,p));
                json!({"mount": id, "path": path, "operation": if entry.is_some() {"write"} else {"delete"}, "entry": entry})
            }).collect();
            Ok(json!(rows))
        }
        Cmd::Map { command } => match command {
            MapCmd::Show { workspace, active } => {
                let contents = if active {
                    toml::to_string_pretty(&s.state(s.head(&workspace)?)?.0)?
                } else {
                    s.draft(&workspace)?
                };
                Ok(json!({"contents": contents}))
            }
            MapCmd::Set { workspace, file } => {
                s.set_draft(&workspace, &std::fs::read_to_string(file)?)?;
                Ok(json!({"staged": workspace}))
            }
            MapCmd::ImportSuperrepo {
                workspace,
                path,
                revision,
            } => import_super(s, &workspace, &path, &revision),
        },
        Cmd::Snapshot { command } => match command {
            SnapshotCmd::Create { workspace } => Ok(json!({"number": s.snapshot(&workspace)?})),
            SnapshotCmd::List { workspace } => Ok(json!(s.snapshots(&workspace)?)),
            SnapshotCmd::Pin { workspace, number } => {
                let n = s.conn()?.execute(
                    "UPDATE snapshots SET pinned=1 WHERE workspace=?1 AND number=?2",
                    rusqlite::params![workspace, number],
                )?;
                if n == 0 {
                    return Err(error("snapshot not found"));
                }
                Ok(json!({"pinned": number}))
            }
        },
        Cmd::Restore { workspace, number } => {
            s.restore(&workspace, number)?;
            Ok(json!({"restored": number}))
        }
        Cmd::Stash { workspace } => {
            let guard = s.workspace_lock(&workspace)?;
            if s.frozen(&workspace)? {
                return Err(error("activation in progress"));
            }
            let number = s.snapshot_unlocked(&workspace)?;
            let (map, _) = s.state(s.head(&workspace)?)?;
            s.publish(&workspace, None, &map, &[])?;
            drop(guard);
            Ok(json!({"stash_snapshot": number}))
        }
        Cmd::Activate {
            workspace,
            plan,
            resume,
            abort,
        } => {
            if plan {
                return Ok(json!({"proposal": s.activate_plan(&workspace)?}));
            }
            if resume || abort {
                let id: Option<String> = s.conn()?.query_row(
                    "SELECT activation FROM workspaces WHERE name=?1",
                    [&workspace],
                    |r| r.get(0),
                )?;
                let id = id.ok_or_else(|| error("no activation in progress"))?;
                s.finish_activation(&id, abort)?;
                return Ok(
                    json!({"transaction": id, "state": if abort {"ABORTED"} else {"COMMITTED"}}),
                );
            }
            let id = s.activate(&workspace)?;
            let conflicts = s.conflict_list(&id)?;
            if conflicts.is_empty() {
                s.finish_activation(&id, false)?;
            }
            Ok(
                json!({"transaction": id, "state": if conflicts.is_empty() {"COMMITTED"} else {"CONFLICTED"}, "conflicts": conflicts}),
            )
        }
        Cmd::Conflicts { transaction } => Ok(json!(s.conflict_list(&transaction)?)),
        Cmd::Resolve {
            transaction,
            mount,
            path,
            discard,
        } => {
            s.resolve_conflict(&transaction, &mount, &path, discard)?;
            Ok(json!({"resolved": path}))
        }
        Cmd::Export {
            workspace,
            destination,
        } => s.export(&workspace, &destination),
        Cmd::Import { workspace, export } => import_export(s, &workspace, &export),
        Cmd::Layout { command } => {
            let path = s.root.join("layouts");
            match command {
                LayoutCmd::Branch { workspace, name } => {
                    if !name_ok(&name) {
                        return Err(error("invalid layout name"));
                    }
                    s.record_layout(&workspace)?;
                    git(
                        &path,
                        &["branch", "-f", &name, &format!("refs/heads/{workspace}")],
                    )?;
                    Ok(json!({"branch": name}))
                }
                LayoutCmd::Switch { workspace, name } => {
                    if !name_ok(&name) {
                        return Err(error("invalid layout name"));
                    }
                    let bytes = git(&path, &["show", &format!("{name}:VERSION_MAP")])?;
                    let mut map = Map::parse(std::str::from_utf8(&bytes)?)?;
                    for m in &mut map.mount {
                        if let Some(commit) = m.commit.take() {
                            m.revision = commit;
                        }
                    }
                    s.set_draft(&workspace, &toml::to_string_pretty(&map)?)?;
                    Ok(json!({"staged_layout": name, "activate_required": true}))
                }
            }
        }
        Cmd::Bundle {
            export,
            destination,
        } => crate::workflow::bundle(&export, &destination),
        Cmd::Review {
            export,
            publish,
            base,
        } => crate::workflow::review(s, &export, publish, base.as_deref()),
        Cmd::Land {
            export,
            verify_only,
        } => crate::workflow::land(s, &export, verify_only),
        Cmd::Gc { dry_run, max_bytes } => s.gc(dry_run, max_bytes),
        Cmd::Mount { .. } | Cmd::Unmount { .. } => Err(error("mount commands are handled locally")),
    }
}

fn import_super(s: &Store, name: &str, path: &Path, revision: &str) -> Result<Value> {
    if revision.starts_with('-') {
        return Err(error("invalid revision"));
    }
    let commit = String::from_utf8(git(
        path,
        &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )?)?
    .trim()
    .to_string();
    let modules = git(
        path,
        &[
            "config",
            "--blob",
            &format!("{commit}:.gitmodules"),
            "--get-regexp",
            "^submodule\\..*\\.path$",
        ],
    )?;
    let mut map = Map::default();
    for line in std::str::from_utf8(&modules)?.lines() {
        let (key, subpath) = line
            .split_once(' ')
            .ok_or_else(|| error("invalid .gitmodules entry"))?;
        let urlkey = key.strip_suffix(".path").unwrap().to_owned() + ".url";
        let url = String::from_utf8(git(
            path,
            &[
                "config",
                "--blob",
                &format!("{commit}:.gitmodules"),
                "--get",
                &urlkey,
            ],
        )?)?
        .trim()
        .to_string();
        let record = String::from_utf8(git(path, &["ls-tree", &commit, "--", subpath])?)?;
        let fields: Vec<_> = record.split_whitespace().collect();
        if fields.len() < 3 || fields[1] != "commit" {
            continue;
        }
        let id = format!("repo-{}", &crate::cache::hash(url.as_bytes())[..12]);
        let exists: bool = s.conn()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM repos WHERE id=?1)",
            [&id],
            |r| r.get(0),
        )?;
        if !exists {
            s.repo_add(&id, &url)?;
        }
        map.mount.push(Mount {
            id: id.clone(),
            path: subpath.into(),
            repo: id,
            revision: fields[2].into(),
            subtree: String::new(),
            commit: None,
        });
    }
    map.validate()?;
    s.set_draft(name, &toml::to_string_pretty(&map)?)?;
    Ok(json!({"staged":name,"source_commit":commit,"mounts":map.mount.len()}))
}
fn import_export(s: &Store, name: &str, path: &Path) -> Result<Value> {
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(path.join("gitalator-export.json"))?)?;
    let revision = manifest["revision"]
        .as_i64()
        .ok_or_else(|| error("invalid export revision"))?;
    if manifest["workspace"].as_str() != Some(name) {
        return Err(error("export belongs to another workspace"));
    }
    let (map, _) = s.state(revision)?;
    let mut changes = vec![];
    for repo in manifest["repositories"]
        .as_array()
        .ok_or_else(|| error("invalid manifest"))?
    {
        let mount = map
            .mount
            .iter()
            .find(|m| Some(m.id.as_str()) == repo["mount"].as_str())
            .ok_or_else(|| error("unknown mount"))?;
        let root = Path::new(repo["path"].as_str().ok_or_else(|| error("missing path"))?);
        let head = repo["head"].as_str().ok_or_else(|| error("missing head"))?;
        let diff = git(root, &["diff", "--name-only", "-z", head, "--"])?;
        let untracked = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
        for p in diff
            .split(|b| *b == 0)
            .chain(untracked.split(|b| *b == 0))
            .filter(|p| !p.is_empty())
        {
            let p = std::str::from_utf8(p)?;
            let rel = if mount.subtree.is_empty() {
                p
            } else {
                p.strip_prefix(&(mount.subtree.clone() + "/"))
                    .ok_or_else(|| error("change outside mounted subtree"))?
            };
            let viewpath = join(&mount.path, rel);
            if !map.owner(&viewpath).is_some_and(|(m, _)| m.id == mount.id) {
                return Err(error("export edit crosses nested ownership"));
            }
            let file = root.join(p);
            let entry = match std::fs::symlink_metadata(&file) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
                Ok(meta) => {
                    use std::os::unix::fs::PermissionsExt;
                    let (kind, b) = if meta.is_symlink() {
                        (
                            Kind::Symlink,
                            std::fs::read_link(&file)?
                                .to_string_lossy()
                                .as_bytes()
                                .to_vec(),
                        )
                    } else {
                        (Kind::File, std::fs::read(&file)?)
                    };
                    Some(Entry {
                        kind,
                        mode: meta.permissions().mode() & 0o777,
                        content: s.cache.put(&b)?,
                        size: b.len() as u64,
                    })
                }
            };
            changes.push((viewpath, entry));
        }
    }
    let id = s.import_changes(name, revision, changes)?;
    let conflicts = s.conflict_list(&id)?;
    if conflicts.is_empty() {
        s.finish_activation(&id, false)?;
    }
    Ok(
        json!({"imported":name,"transaction":id,"state":if conflicts.is_empty(){"COMMITTED"}else{"CONFLICTED"},"conflicts":conflicts}),
    )
}
pub fn request(root: &Path, cmd: &Cmd) -> Result<Option<Value>> {
    let socket = root.join("control.sock");
    if !socket.exists() {
        return Ok(None);
    }
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.write_all(&serde_json::to_vec(
        &json!({"command": cmd, "progress": true}),
    )?)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    for line in std::io::BufReader::new(stream).lines() {
        let value: Value = serde_json::from_str(&line?)?;
        if let Some(progress) = value.get("progress").and_then(Value::as_str) {
            eprintln!("gtl: {progress}");
            continue;
        }
        if let Some(e) = value.get("error") {
            return Err(error(e.as_str().unwrap_or("daemon error")));
        }
        return Ok(Some(value["result"].clone()));
    }
    Err(error("daemon disconnected before returning a result"))
}
pub fn server(
    store: Store,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};
    store
        .runtime
        .managed
        .store(true, std::sync::atomic::Ordering::Release);
    let p = store.root.join("control.sock");
    if p.exists() {
        std::fs::remove_file(&p)?;
    }
    let listener = UnixListener::bind(&p)?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(std::thread::spawn(move || {
        let mut jobs = vec![];
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    if jobs.len() >= 16 {
                        let _ = stream.write_all(b"{\"error\":\"control queue busy; retry\"}");
                        continue;
                    }
                    let mut s = store.clone();
                    jobs.push(std::thread::spawn(move || {
                        let result = (|| -> Result<Value> {
                            let mut data = vec![];
                            stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
                            (&mut stream).take(1024 * 1024).read_to_end(&mut data)?;
                            let request: Value = serde_json::from_slice(&data)?;
                            if request.get("progress").and_then(Value::as_bool) == Some(true) {
                                // A disconnected or stalled progress consumer must not hold up
                                // Git pipe drainage or defeat the backend deadline.
                                stream
                                    .set_write_timeout(Some(std::time::Duration::from_secs(1)))?;
                                let writer = std::sync::Mutex::new(Some(stream.try_clone()?));
                                s.progress = std::sync::Arc::new(move |line| {
                                    if let Ok(mut writer) = writer.lock() {
                                        if let Some(socket) = writer.as_mut() {
                                            if writeln!(socket, "{}", json!({"progress":line}))
                                                .is_err()
                                            {
                                                *writer = None;
                                            }
                                        }
                                    }
                                });
                                dispatch(&s, serde_json::from_value(request["command"].clone())?)
                            } else {
                                dispatch(&s, serde_json::from_value(request)?)
                            }
                        })();
                        let output = match result {
                            Ok(v) => json!({"result":v}),
                            Err(e) => json!({"error":e.to_string()}),
                        };
                        let _ = stream.write_all(output.to_string().as_bytes());
                    }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(50))
                }
                Err(e) => {
                    eprintln!("control accept: {e}");
                    break;
                }
            }
            jobs.retain(|j| !j.is_finished());
        }
        for j in jobs {
            let _ = j.join();
        }
        let _ = std::fs::remove_file(p);
    }))
}
impl Cmd {
    /// Resolve client-side paths before sending requests to a daemon with a different cwd.
    pub fn absolutize(&mut self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let absolute = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = cwd.join(&*p);
            }
        };
        match self {
            Self::Mount { path } | Self::Unmount { path } => absolute(path),
            Self::Workspace {
                command:
                    WorkspaceCmd::Create {
                        map: Some(path), ..
                    },
            } => absolute(path),
            Self::Map {
                command: MapCmd::Set { file, .. },
            } => absolute(file),
            Self::Map {
                command: MapCmd::ImportSuperrepo { path, .. },
            } => absolute(path),
            Self::Export { destination, .. } => absolute(destination),
            Self::Import { export, .. }
            | Self::Review { export, .. }
            | Self::Land { export, .. } => absolute(export),
            Self::Bundle {
                export,
                destination,
            } => {
                absolute(export);
                absolute(destination);
            }
            Self::Repo {
                command: RepoCmd::Add { origin, .. },
            } if Path::new(origin).exists() => {
                *origin = std::fs::canonicalize(&*origin)?
                    .to_string_lossy()
                    .into_owned();
            }
            _ => {}
        }
        Ok(())
    }
}
