use crate::{
    backend::git,
    cache::{atomic, Span},
    db::Store,
    error,
    model::*,
    ops::View,
    Result,
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
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
    Doctor,
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
    if result.is_err() {
        span.event("failed", Some("command failed; see client error"));
    }
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
    match cmd{
 Cmd::Init=>Ok(json!({"state_dir":s.root,"schema":1})),
 Cmd::Doctor=>Ok(json!({"database":s.conn()?.query_row("PRAGMA integrity_check",[],|r|r.get::<_,String>(0))?,"git":String::from_utf8(git(&s.root,&["--version"])?)?,"fuse_device":Path::new("/dev/fuse").exists()})),
 Cmd::Repo{command}=>match command{RepoCmd::Add{id,origin}=>{let origin=if Path::new(&origin).exists(){std::fs::canonicalize(&origin)?.to_string_lossy().into_owned()}else{origin};s.repo_add(&id,&origin)?;Ok(json!({"registered":id}))},RepoCmd::Fetch{id}=>{s.fetch(&id)?;Ok(json!({"fetched":id}))},RepoCmd::List=>{let c=s.conn()?;let mut q=c.prepare("SELECT id FROM repos ORDER BY id")?;let ids=q.query_map([],|r|r.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;Ok(json!(ids))}},
 Cmd::Workspace{command}=>match command{WorkspaceCmd::List=>Ok(json!(s.names()?)),WorkspaceCmd::Create{name,map,from,snapshot}=>{if let Some(source)=from{let revision=if let Some(number)=snapshot{s.snapshot_revision(&source,number)?.0}else{s.head(&source)?};let draft=if let Some(number)=snapshot{s.snapshot_revision(&source,number)?.1}else{s.draft(&source)?};s.create(&name,&draft)?;s.conn()?.execute("UPDATE workspaces SET head=?2 WHERE name=?1",rusqlite::params![name,revision])?;}else{s.create(&name,&if let Some(path)=map{std::fs::read_to_string(path)?}else{"format = 1\n".into()})?;}Ok(json!({"workspace":name}))}},
 Cmd::Status{workspace}=>{let(m,o)=s.state(s.head(&workspace)?)?;let draft=s.draft(&workspace)?;let draft_map=Map::parse(&draft);let mut active=m.clone();for m in &mut active.mount{m.commit=None;}Ok(json!({"workspace":workspace,"head":s.head(&workspace)?,"frozen":s.frozen(&workspace)?,"overlay_entries":o.len(),"draft_valid":draft_map.is_ok(),"draft_differs":draft_map.map(|m|serde_json::to_string(&m).ok()!=serde_json::to_string(&active).ok()).unwrap_or(true)}))},
 Cmd::Diff{workspace}=>{let(m,o)=s.state(s.head(&workspace)?)?;Ok(json!(o.iter().map(|((id,p),e)|json!({"mount":id,"path":m.mount.iter().find(|m|&m.id==id).map(|m|join(&m.path,p)),"operation":if e.is_some(){"write"}else{"delete"},"entry":e})).collect::<Vec<_>>()))},
 Cmd::Map{command}=>match command{MapCmd::Show{workspace,active}=>Ok(json!({"contents":if active{toml::to_string_pretty(&s.state(s.head(&workspace)?)?.0)?}else{s.draft(&workspace)?}})),MapCmd::Set{workspace,file}=>{s.set_draft(&workspace,&std::fs::read_to_string(file)?)?;Ok(json!({"staged":workspace}))},MapCmd::ImportSuperrepo{workspace,path,revision}=>import_super(s,&workspace,&path,&revision)},
 Cmd::Snapshot{command}=>match command{SnapshotCmd::Create{workspace}=>Ok(json!({"number":s.snapshot(&workspace)?})),SnapshotCmd::List{workspace}=>Ok(json!(s.snapshots(&workspace)?)),SnapshotCmd::Pin{workspace,number}=>{let count=s.conn()?.execute("UPDATE snapshots SET pinned=1 WHERE workspace=?1 AND number=?2",rusqlite::params![workspace,number])?;if count==0{return Err(error("snapshot not found"));}Ok(json!({"pinned":number}))}},
 Cmd::Restore{workspace,number}=>{s.restore(&workspace,number)?;Ok(json!({"restored":number}))},
 Cmd::Stash{workspace}=>{let number=s.snapshot(&workspace)?;let _guard=s.workspace_lock(&workspace)?;if s.frozen(&workspace)?{return Err(error("activation in progress"));}let(m,_)=s.state(s.head(&workspace)?)?;s.publish(&workspace,None,&m,&[])?;Ok(json!({"stash_snapshot":number}))},
 Cmd::Activate{workspace,plan,resume,abort}=>{if plan{return Ok(json!({"proposal":s.activate_plan(&workspace)?}));}if resume||abort{let id:Option<String>=s.conn()?.query_row("SELECT activation FROM workspaces WHERE name=?1",[&workspace],|r|r.get(0))?;let id=id.ok_or_else(||error("no activation in progress"))?;s.finish_activation(&id,abort)?;return Ok(json!({"transaction":id,"state":if abort{"ABORTED"}else{"COMMITTED"}}));}let id=s.activate(&workspace)?;let conflicts=s.conflict_list(&id)?;if conflicts.is_empty(){s.finish_activation(&id,false)?;}Ok(json!({"transaction":id,"state":if conflicts.is_empty(){"COMMITTED"}else{"CONFLICTED"},"conflicts":conflicts}))},
 Cmd::Conflicts{transaction}=>Ok(json!(s.conflict_list(&transaction)?)),
 Cmd::Resolve{transaction,mount,path,discard}=>{s.resolve_conflict(&transaction,&mount,&path,discard)?;Ok(json!({"resolved":path}))},
 Cmd::Export{workspace,destination}=>s.export(&workspace,&destination),
 Cmd::Import{workspace,export}=>import_export(s,&workspace,&export),
 Cmd::Layout{command}=>{let path=s.root.join("layouts");match command{LayoutCmd::Branch{workspace,name}=>{if !name_ok(&name){return Err(error("invalid layout name"));}s.record_layout(&workspace)?;git(&path,&["branch","-f",&name,"HEAD"])?;Ok(json!({"branch":name}))},LayoutCmd::Switch{workspace,name}=>{if !name_ok(&name){return Err(error("invalid layout name"));}let data=git(&path,&["show",&format!("{name}:VERSION_MAP")])?;let mut map=Map::parse(std::str::from_utf8(&data)?)?;for m in &mut map.mount{if let Some(c)=m.commit.take(){m.revision=c;}}s.set_draft(&workspace,&toml::to_string_pretty(&map)?)?;Ok(json!({"staged_layout":name,"activate_required":true}))}}},
 Cmd::Bundle{export,destination}=>{let manifest:Value=serde_json::from_slice(&std::fs::read(export.join("gitalator-export.json"))?)?;if destination.exists(){return Err(error("bundle destination exists"));}std::fs::create_dir_all(&destination)?;let dest=std::fs::canonicalize(destination)?;for repo in manifest["repositories"].as_array().ok_or_else(||error("invalid manifest"))?{let path=Path::new(repo["path"].as_str().ok_or_else(||error("missing path"))?);let name=repo["mount"].as_str().unwrap();if !name_ok(name){return Err(error("invalid mount"));}git(path,&["bundle","create",dest.join(format!("{name}.bundle")).to_str().unwrap(),"HEAD"])?;git(path,&["bundle","verify",dest.join(format!("{name}.bundle")).to_str().unwrap()])?;}atomic(&dest.join("gitalator-export.json"),&serde_json::to_vec_pretty(&manifest)?)?;Ok(json!({"bundle_directory":dest}))},
 Cmd::Review{..}=>Err(error("review adapter not implemented; use the exported ordinary Git worktrees and your silo's review tool")),
 Cmd::Gc{dry_run,max_bytes}=>s.gc(dry_run,max_bytes),
 Cmd::Mount{..}|Cmd::Unmount{..}=>Err(error("mount commands are handled locally")),
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
    if s.head(name)? != revision {
        return Err(error("workspace advanced since export; import requires explicit reconciliation (see backlog)"));
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
    s.mutate_many(&View::Live(name.into()), changes)?;
    Ok(json!({"imported":name}))
}
pub fn request(root: &Path, cmd: &Cmd) -> Result<Option<Value>> {
    let socket = root.join("control.sock");
    if !socket.exists() {
        return Ok(None);
    }
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.write_all(&serde_json::to_vec(cmd)?)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut data = vec![];
    stream.read_to_end(&mut data)?;
    let value: Value = serde_json::from_slice(&data)?;
    if let Some(e) = value.get("error") {
        return Err(error(e.as_str().unwrap_or("daemon error")));
    }
    Ok(Some(value["result"].clone()))
}
pub fn server(
    store: Store,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};
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
                    let s = store.clone();
                    jobs.push(std::thread::spawn(move || {
                        let result = (|| -> Result<Value> {
                            let mut data = vec![];
                            stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
                            (&mut stream).take(1024 * 1024).read_to_end(&mut data)?;
                            dispatch(&s, serde_json::from_slice(&data)?)
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
