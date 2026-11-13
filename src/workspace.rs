use crate::vfs::Core;
use crate::{
    cli::{Cmd, DepotCmd, WorkspaceCmd},
    error, git,
    model::*,
    overlay::io,
    store::{copy_tree, remove_any, token, Store, Workspace},
    Result,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};
impl Workspace {
    pub fn quiescent(&self) -> Result<()> {
        self.check()?;
        if self.handles != 0 {
            return Err(io(libc::EBUSY));
        }
        Ok(())
    }
    pub fn begin(&mut self) -> Result<String> {
        self.quiescent()?;
        let token = token();
        self.record.editing = true;
        self.record.edit_token = Some(token.clone());
        self.save()?;
        Ok(token)
    }
    pub fn end(&mut self, token_: &str) -> Result<()> {
        if !self.record.editing || self.record.edit_token.as_deref() != Some(token_) {
            return Err(error("invalid edit-session token"));
        }
        self.validate()?;
        if git::text(&self.depot(), &["symbolic-ref", "--short", "HEAD"])? != self.record.branch {
            return Err(error(
                "workspace branch changed; restore it before resuming",
            ));
        }
        self.record.active_commit = git::text(&self.depot(), &["rev-parse", "HEAD"])?;
        self.record.generation = token();
        self.record.editing = false;
        self.record.initializing = false;
        self.record.edit_token = None;
        self.save()
    }
    pub fn checkpoint(&mut self, message: &str) -> Result<String> {
        self.quiescent()?;
        self.validate()?;
        let staged = git::run(&self.depot(), &["show", ":depot.toml"])?;
        if Map::parse(std::str::from_utf8(&staged)?)? != self.record.map {
            return Err(error("staged layout differs from active map"));
        }
        // Export the staged tree to an isolated validation worktree without mutating
        // its index or copying unstaged bytes into the checkpoint.
        let tree = git::text(&self.depot(), &["write-tree"])?;
        let candidate = self.root.join(format!("validate-{}", token()));
        fs::create_dir(&candidate)?;
        let result = (|| {
            git::run(
                &self.depot(),
                &[
                    "checkout-index",
                    "--all",
                    &format!("--prefix={}/", candidate.display()),
                ],
            )?;
            let fake = self.root.join(format!("candidate-{}", token()));
            fs::create_dir(&fake)?;
            fs::rename(&candidate, fake.join("depot"))?;
            let w = Workspace {
                root: fake.clone(),
                record: self.record.clone(),
                namespace: self.namespace.clone(),
                handles: 0,
                listed: Default::default(),
                disabled_batches: Default::default(),
                metadata: self.metadata.clone(),
            };
            let r = w.validate();
            remove_any(&fake)?;
            r
        })();
        if candidate.exists() {
            remove_any(&candidate)?;
        }
        result?;
        let staged_entries = git::run(&self.depot(), &["ls-files", "--stage", "-z"])?;
        let staged_tombs: Vec<String> = staged_entries
            .split(|b| *b == 0)
            .filter_map(|row| {
                std::str::from_utf8(row)
                    .ok()?
                    .split_once('\t')?
                    .1
                    .strip_prefix("tombstones/")
                    .map(str::to_string)
            })
            .collect();
        let mut normalize = vec![];
        for row in staged_entries.split(|b| *b == 0).filter(|b| !b.is_empty()) {
            let row = std::str::from_utf8(row)?;
            let (meta, path) = row.split_once('\t').ok_or("invalid index")?;
            let f: Vec<_> = meta.split(' ').collect();
            if f[2] != "0" {
                return Err(error("unmerged index"));
            }
            if let Some(p) = path.strip_prefix("overlay/") {
                if staged_tombs.iter().any(|t| under(p, t)) {
                    continue;
                }
                if let Some(base) = self.namespace.lookup(p)? {
                    let mode = match base.kind {
                        Kind::File => {
                            if base.mode & 0o111 != 0 {
                                "100755"
                            } else {
                                "100644"
                            }
                        }
                        Kind::Symlink => "120000",
                        _ => continue,
                    };
                    if mode == f[0]
                        && (base.oid == f[1]
                            || git::run(&self.depot(), &["cat-file", "blob", f[1]])?
                                == fs::read(self.base_file(p)?)?)
                    {
                        normalize.push((path.to_string(), p.to_string()));
                    }
                }
            }
        }
        let session = self.begin()?;
        let result = (|| {
            for (path, _) in &normalize {
                git::run(
                    &self.depot(),
                    &["update-index", "--force-remove", "--", path],
                )?;
            }
            let normalized = git::text(&self.depot(), &["write-tree"])?;
            let current = git::text(&self.depot(), &["rev-parse", "HEAD^{tree}"])?;
            if normalized != current {
                if let Err(e) = git::run(&self.depot(), &["commit", "-m", message]) {
                    git::run(&self.depot(), &["read-tree", &tree])?;
                    self.end(&session)?;
                    return Err(e);
                }
            }
            crate::failpoint("checkpoint-committed");
            for (_, p) in &normalize {
                if let Ok(e) = self.entry_unchecked(p) {
                    if !self.hidden(p)? && self.equal_base(p, &e)? {
                        remove_any(&self.upper().join(p))?;
                    }
                }
            }
            self.end(&session)?;
            git::text(&self.depot(), &["rev-parse", "HEAD"])
        })();
        // Unexpected failures retain the persisted edit lease for explicit repair.
        result
    }
    fn entry_unchecked(&mut self, p: &str) -> Result<crate::overlay::Entry> {
        let editing = self.record.editing;
        self.record.editing = false;
        let e = self.entry(p);
        self.record.editing = editing;
        e
    }
}
impl Store {
    pub fn fork(&mut self, name: &str, source: &str, live: bool) -> Result<String> {
        let w = self.get(source)?;
        let w = crate::activity::lock(&w)?;
        w.quiescent()?;
        w.validate()?;
        let head = git::text(&w.depot(), &["rev-parse", "HEAD"])?;
        // Fork the existing managed store directly, including depots with no remote.
        let path = self.create_from_repo(name, w.record.depot_repo.clone(), head)?;
        if live {
            let target = self.get(name)?;
            let mut target = crate::activity::lock(&target)?;
            target.record.editing = true;
            target.record.edit_token = Some(token());
            target.save()?;
            let index = git::text(&w.depot(), &["rev-parse", "--git-path", "index"])?;
            let target_index = git::text(&target.depot(), &["rev-parse", "--git-path", "index"])?;
            fs::copy(Path::new(&index), Path::new(&target_index))?;
            // Git supplies the tracked and nonignored untracked set, including arbitrary
            // depot configuration files. Missing tracked paths reproduce dirty deletions.
            let entries = git::run(
                &w.depot(),
                &[
                    "ls-files",
                    "-z",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                ],
            )?;
            for e in fs::read_dir(target.depot())? {
                let e = e?;
                if e.file_name() != ".git" {
                    remove_any(&e.path())?;
                }
            }
            for p in entries.split(|b| *b == 0).filter(|b| !b.is_empty()) {
                let p = std::str::from_utf8(p)?;
                if !valid_path(p) {
                    return Err(error("invalid tracked path"));
                }
                let src = w.depot().join(p);
                let dst = target.depot().join(p);
                if let Ok(m) = fs::symlink_metadata(&src) {
                    fs::create_dir_all(dst.parent().unwrap())?;
                    if m.file_type().is_symlink() {
                        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
                    } else if m.is_file() {
                        fs::copy(src, dst)?;
                    } else if m.is_dir() {
                        copy_tree(&src, &dst)?;
                    }
                }
            }
            let t = target.record.edit_token.clone().unwrap();
            target.record.map = w.record.map.clone();
            target.namespace = w.namespace.clone();
            target.save()?;
            target.end(&t)?;
        }
        Ok(path)
    }
    pub fn remove(&mut self, name: &str) -> Result<()> {
        let w = self.get(name)?;
        let w = crate::activity::lock(&w)?;
        w.quiescent()?;
        if !git::run(
            &w.depot(),
            &["status", "--porcelain", "--untracked-files=all"],
        )?
        .is_empty()
        {
            return Err(error("workspace has dirty or untracked files"));
        }
        if !git::run(
            &w.depot(),
            &["ls-files", "--others", "--ignored", "--exclude-standard"],
        )?
        .is_empty()
        {
            return Err(error(
                "workspace has ignored files; remove them explicitly first",
            ));
        }
        let bare = self.root.join("repos").join(&w.record.depot_repo);
        let _l = crate::util::Lock::take(&self.root.join("locks").join(&w.record.depot_repo))?;
        git::run(&bare, &["worktree", "remove", w.depot().to_str().unwrap()])?;
        remove_any(&w.root)?;
        drop(w);
        self.workspaces.remove(name);
        Ok(())
    }
}
pub fn dispatch(core: &Arc<Core>, cmd: Cmd) -> Result<Value> {
    match cmd {
        Cmd::Mount { mut command } => {
            let (_, workspace, _) = command.location_mut();
            let name = workspace
                .clone()
                .ok_or("mount command requires a workspace")?;
            let store = crate::activity::lock(&core.store)?.clone();
            use crate::cli::MountCmd;
            let result = match command {
                MountCmd::Add {
                    url,
                    at,
                    revision,
                    submodules,
                    skip_unavailable,
                    workspace_id,
                    ..
                } => store.add_mount_with_options(
                    &name,
                    workspace_id.as_deref(),
                    &url,
                    &at,
                    &revision,
                    submodules,
                    skip_unavailable,
                ),
                MountCmd::Remove {
                    at,
                    recursive,
                    workspace_id,
                    ..
                } => store.remove_mount(&name, workspace_id.as_deref(), &at, recursive),
                MountCmd::Submodules {
                    at,
                    depth,
                    skip_unavailable,
                    workspace_id,
                    ..
                } => store.expand_submodules_with_options(
                    &name,
                    workspace_id.as_deref(),
                    &at,
                    depth.into(),
                    skip_unavailable,
                ),
            }?;
            core.invalidate(&name)?;
            Ok(result)
        }
        Cmd::Workspace { command } => {
            let mut s = crate::activity::lock(&core.store)?;
            match command {
                WorkspaceCmd::Init { name } => {
                    let path = s.init(&name)?;
                    let w = s.get(&name)?;
                    let w = crate::activity::lock(&w)?;
                    Ok(
                        json!({"path":path,"depot_worktree":w.depot(),"depot_repository":s.root.join("repos").join(&w.record.depot_repo),"branch":w.record.branch}),
                    )
                }
                WorkspaceCmd::Create {
                    name,
                    from,
                    revision,
                } => Ok(json!({"path":s.create(&name,&from,&revision)?})),
                WorkspaceCmd::Fork { name, from } => {
                    Ok(json!({"path":s.fork(&name,&from,false)?,"dirty_state_copied":false}))
                }
                WorkspaceCmd::Clone { name, from } => {
                    Ok(json!({"path":s.fork(&name,&from,true)?,"ignored_outputs_copied":false}))
                }
                WorkspaceCmd::Remove { name } => {
                    s.remove(&name)?;
                    Ok(json!({"removed":name,"branch_retained":true}))
                }
                WorkspaceCmd::List => Ok(json!(s.workspaces.keys().collect::<Vec<_>>())),
            }
        }
        Cmd::Config { mount } => {
            let mut s = crate::activity::lock(&core.store)?;
            if core.notifier.lock().unwrap().is_some() {
                return Err(error(
                    "mount configuration requires stopped daemon and --offline",
                ));
            }
            let mount = crate::store::absolute(&mount)?;
            if mount.starts_with(&s.root) && mount != s.root.join("mount") {
                return Err(error("mount may not hide backing state"));
            }
            s.config.mount = mount;
            crate::store::json_write(&s.root.join("config.json"), &s.config)?;
            Ok(json!(s.config))
        }
        Cmd::Dir { workspace } => {
            Ok(json!({"path":crate::activity::lock(&core.store)?.config.mount.join(workspace)}))
        }
        Cmd::Status { workspace } => {
            let w = get(core, &workspace)?;
            let w = crate::activity::lock(&w)?;
            Ok(
                json!({"record":w.record,"worktree":w.depot(),"tree_cache":w.namespace.trees.stats(),"git_status":git::text(&w.depot(),&["status","--porcelain"]).unwrap_or_else(|e|format!("unavailable: {e}"))}),
            )
        }
        Cmd::Diff { workspace } => crate::activity::lock(&*get(core, &workspace)?)?.diff(),
        Cmd::Validate { workspace, full } => {
            let arc = get(core, &workspace)?;
            let w = crate::activity::lock(&arc)?;
            w.validate()?;
            if full {
                w.namespace.validate_full()?;
            }
            Ok(json!({"valid":true}))
        }
        Cmd::Revert { workspace, path } => {
            let w = get(core, &workspace)?;
            let w = crate::activity::lock(&w)?;
            w.quiescent()?;
            w.revert(&path)?;
            drop(w);
            core.invalidate(&workspace)?;
            Ok(json!({"reverted":path}))
        }
        Cmd::Checkpoint { workspace, message } => {
            let commit = crate::activity::lock(&*get(core, &workspace)?)?.checkpoint(&message)?;
            core.invalidate(&workspace)?;
            Ok(json!({"commit":commit}))
        }
        Cmd::Depot { command } => match command {
            DepotCmd::Begin { workspace } => {
                let w = get(core, &workspace)?;
                let mut w = crate::activity::lock(&w)?;
                let t = w.begin()?;
                Ok(json!({"token":t,"worktree":w.depot()}))
            }
            DepotCmd::End { workspace, token } => {
                crate::activity::lock(&*get(core, &workspace)?)?.end(&token)?;
                core.invalidate(&workspace)?;
                Ok(json!({"resumed":true}))
            }
            DepotCmd::Edit { .. } => Err(error("edit subprocess must be launched by the client")),
        },
        _ => Err(error("daemon command must be handled by service")),
    }
}
fn get(core: &Core, name: &str) -> Result<Arc<Mutex<Workspace>>> {
    crate::activity::lock(&core.store)?.get(name)
}
