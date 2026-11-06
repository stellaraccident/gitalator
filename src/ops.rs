use crate::{
    backend::git,
    cache::{atomic, hash, Lock, Span},
    db::Store,
    error,
    model::*,
    Result,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conflict {
    pub path: String,
    pub mount: String,
    pub reason: String,
    pub resolved: bool,
}
#[derive(Clone, Debug)]
pub enum View {
    Live(String),
    Base(String),
    Snapshot(String, i64, bool),
    Transaction(String),
}
fn deltas(o: &Overlay) -> Vec<Delta> {
    o.iter()
        .map(|((m, p), e)| Delta {
            mount: m.clone(),
            path: p.clone(),
            entry: e.clone(),
        })
        .collect()
}
fn overlay(d: Vec<Delta>) -> Overlay {
    d.into_iter()
        .map(|d| ((d.mount, d.path), d.entry))
        .collect()
}
pub fn directory() -> Entry {
    Entry {
        kind: Kind::Directory,
        mode: 0o755,
        content: String::new(),
        size: 0,
    }
}
impl Store {
    pub fn view_state(&self, v: &View) -> Result<(Map, Overlay)> {
        match v {
            View::Live(n) => self.state(self.head(n)?),
            View::Base(n) => {
                let (m, _) = self.state(self.head(n)?)?;
                Ok((m, Overlay::new()))
            }
            View::Snapshot(n, num, base) => {
                let (r, _) = self.snapshot_revision(n, *num)?;
                let (m, o) = self.state(r)?;
                Ok((m, if *base { Overlay::new() } else { o }))
            }
            View::Transaction(id) => {
                let (m, d): (String, String) = self.conn()?.query_row(
                    "SELECT proposal,candidate FROM activations WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                Ok((
                    serde_json::from_str(&m)?,
                    overlay(serde_json::from_str(&d)?),
                ))
            }
        }
    }
    pub fn compose(&self, map: &Map, o: &Overlay) -> Result<BTreeMap<String, Entry>> {
        let mut all = BTreeMap::new();
        all.insert(String::new(), directory());
        for m in &map.mount {
            all.insert(m.path.clone(), directory());
            for (p, e) in self.tree(m)? {
                let path = join(&m.path, &p);
                if map.owner(&path).is_some_and(|(owner, _)| owner.id == m.id) {
                    all.insert(path, e);
                }
            }
            let mut parent = Path::new(&m.path).parent();
            while let Some(p) = parent {
                all.entry(p.to_string_lossy().into_owned())
                    .or_insert_with(directory);
                parent = p.parent();
            }
        }
        for ((id, p), entry) in o {
            if let Some(m) = map.mount.iter().find(|m| &m.id == id) {
                let path = join(&m.path, p);
                if !map.owner(&path).is_some_and(|(owner, _)| owner.id == *id) {
                    continue;
                }
                match entry {
                    Some(e) => {
                        all.insert(path, e.clone());
                    }
                    None => {
                        all.remove(&path);
                    }
                }
            }
        }
        Ok(all)
    }
    pub fn entries(&self, v: &View) -> Result<BTreeMap<String, Entry>> {
        let (m, o) = self.view_state(v)?;
        self.compose(&m, &o)
    }
    pub fn entry(&self, v: &View, path: &str) -> Result<Entry> {
        self.entries(v)?
            .remove(path)
            .ok_or_else(|| error("not found"))
    }
    pub fn read(&self, v: &View, path: &str) -> Result<Vec<u8>> {
        let e = self.entry(v, path)?;
        if e.kind == Kind::Directory {
            return Err(error("is a directory"));
        }
        self.bytes(&e)
    }
    pub fn mutate(&self, v: &View, path: &str, entry: Option<Entry>) -> Result<()> {
        self.mutate_many(v, vec![(path.into(), entry)])
    }
    pub fn mutate_many(&self, v: &View, changes: Vec<(String, Option<Entry>)>) -> Result<()> {
        let lockname = match v {
            View::Live(n) => n.clone(),
            View::Transaction(id) => format!("tx-{id}"),
            _ => return Err(error("read-only view")),
        };
        let _guard = self.workspace_lock(&lockname)?;
        if let View::Live(n) = v {
            if self.frozen(n)? {
                return Err(error("workspace activation in progress: writes frozen"));
            }
        }
        let (m, mut o) = self.view_state(v)?;
        let mut ds = vec![];
        for (p, e) in changes {
            if !path_ok(&p) || p.is_empty() || m.boundary(&p) {
                return Err(error("protected mount boundary or invalid path"));
            }
            let (owner, rel) = m
                .owner(&p)
                .ok_or_else(|| error("path has no owning mount"))?;
            ds.push(Delta {
                mount: owner.id.clone(),
                path: rel.clone(),
                entry: e.clone(),
            });
            o.insert((owner.id.clone(), rel), e);
        }
        match v {
            View::Live(n) => {
                self.publish(n, Some(self.head(n)?), &m, &ds)?;
            }
            View::Transaction(id) => {
                let state: String = self.conn()?.query_row(
                    "SELECT state FROM activations WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                if state != "CONFLICTED" && state != "READY" {
                    return Err(error("transaction is not editable"));
                }
                self.conn()?.execute(
                    "UPDATE activations SET candidate=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&deltas(&o))?],
                )?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }
    pub fn write_file(
        &self,
        v: &View,
        path: &str,
        data: &[u8],
        mode: u32,
        kind: Kind,
    ) -> Result<()> {
        let content = self.cache.put(data)?;
        self.mutate(
            v,
            path,
            Some(Entry {
                kind,
                mode,
                content,
                size: data.len() as u64,
            }),
        )
    }
    pub fn restore(&self, name: &str, number: i64) -> Result<()> {
        let _guard = self.workspace_lock(name)?;
        if self.frozen(name)? {
            return Err(error("activation in progress"));
        }
        self.snapshot_unlocked(name)?;
        let (r, d) = self.snapshot_revision(name, number)?;
        let (m, o) = self.state(r)?;
        self.publish(name, None, &m, &deltas(&o))?;
        self.set_draft(name, &d)?;
        self.snapshot_unlocked(name)?;
        Ok(())
    }
    pub fn activate_plan(&self, name: &str) -> Result<Map> {
        self.resolve_map(&Map::parse(&self.draft(name)?)?)
    }
    pub fn activate(&self, name: &str) -> Result<String> {
        let (id, oldmap, oldoverlay, newmap) = {
            let proposed = self.activate_plan(name)?;
            let _g = self.workspace_lock(name)?;
            if self.frozen(name)? {
                return Err(error("activation already in progress"));
            }
            let captured = Map::parse(&self.draft(name)?)?;
            let mut unresolved = proposed.clone();
            for m in &mut unresolved.mount {
                m.commit = None;
            }
            if serde_json::to_string(&captured)? != serde_json::to_string(&unresolved)? {
                return Err(error("draft changed during planning; retry"));
            }
            self.snapshot_unlocked(name)?;
            let head = self.head(name)?;
            let (oldmap, oldoverlay) = self.state(head)?;
            let id = format!(
                "{}-{}-{}",
                name,
                head,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            );
            let mut c = self.conn()?;
            let t = c.transaction()?;
            t.execute("INSERT INTO activations(id,workspace,source,proposal,candidate,conflicts,state) VALUES(?1,?2,?3,?4,'[]','[]','PREPARING')",params![id,name,head,serde_json::to_string(&proposed)?])?;
            t.execute(
                "UPDATE workspaces SET activation=?2 WHERE name=?1",
                params![name, id],
            )?;
            t.commit()?;
            (id, oldmap, oldoverlay, proposed)
        };
        let span = Span::new(&self.root, &format!("activate:{id}"));
        let result = (|| -> Result<()> {
            let mut candidate = Overlay::new();
            let mut conflicts = vec![];
            for ((mount_id, path), ours) in oldoverlay {
                let old = oldmap
                    .mount
                    .iter()
                    .find(|m| m.id == mount_id)
                    .ok_or_else(|| error("orphan overlay"))?;
                let new = newmap.mount.iter().find(|m| m.id == mount_id);
                let mut reason = None;
                let mut chosen = ours.clone();
                if let Some(new) = new {
                    if old.repo != new.repo || old.subtree != new.subtree {
                        reason = Some("backing identity changed");
                    } else if !newmap
                        .owner(&join(&new.path, &path))
                        .is_some_and(|(m, _)| m.id == mount_id)
                    {
                        reason = Some("edit is shadowed by nested mount");
                    } else {
                        let before = self.tree(old)?.remove(&path);
                        let after = self.tree(new)?.remove(&path);
                        if !self.same(&before, &after)? && !self.same(&ours, &after)? {
                            if self.same(&ours, &before)? {
                                continue;
                            }
                            match (&before, &ours, &after) {
                                (Some(b), Some(o), Some(a))
                                    if b.kind == Kind::File
                                        && o.kind == Kind::File
                                        && a.kind == Kind::File =>
                                {
                                    if o.mode != b.mode && a.mode != b.mode && o.mode != a.mode {
                                        reason = Some("conflicting mode changes");
                                    } else {
                                        let (bd, od, ad) =
                                            (self.bytes(b)?, self.bytes(o)?, self.bytes(a)?);
                                        if bd.contains(&0) || od.contains(&0) || ad.contains(&0) {
                                            reason = Some("binary conflict");
                                        } else {
                                            let dir = self.root.join("transactions").join(&id);
                                            std::fs::create_dir_all(&dir)?;
                                            let tmp = dir.join(hash(
                                                format!("{mount_id}/{path}").as_bytes(),
                                            ));
                                            std::fs::create_dir_all(&tmp)?;
                                            atomic(&tmp.join("ours"), &od)?;
                                            atomic(&tmp.join("base"), &bd)?;
                                            atomic(&tmp.join("theirs"), &ad)?;
                                            let out = std::process::Command::new("git")
                                                .args([
                                                    "merge-file",
                                                    "-p",
                                                    "ours",
                                                    "base",
                                                    "theirs",
                                                ])
                                                .current_dir(&tmp)
                                                .output()?;
                                            if out.status.code().is_none_or(|c| c > 127) {
                                                return Err(error("git merge-file failed"));
                                            }
                                            if !out.status.success() {
                                                reason = Some("text conflict");
                                            }
                                            chosen = Some(Entry {
                                                kind: Kind::File,
                                                mode: if o.mode == b.mode {
                                                    a.mode
                                                } else {
                                                    o.mode
                                                },
                                                content: self.cache.put(&out.stdout)?,
                                                size: out.stdout.len() as u64,
                                            });
                                        }
                                    }
                                }
                                _ => reason = Some("delete/type/add conflict"),
                            }
                        } else if self.same(&ours, &after)? {
                            continue;
                        }
                    }
                } else {
                    reason = Some("mount removed; export or explicitly discard this edit");
                }
                if let Some(reason) = reason {
                    conflicts.push(Conflict {
                        path: path.clone(),
                        mount: mount_id.clone(),
                        reason: reason.into(),
                        resolved: false,
                    });
                }
                candidate.insert((mount_id, path), chosen);
            }
            let state = if conflicts.is_empty() {
                "READY"
            } else {
                "CONFLICTED"
            };
            self.conn()?.execute(
                "UPDATE activations SET candidate=?2,conflicts=?3,state=?4 WHERE id=?1",
                params![
                    id,
                    serde_json::to_string(&deltas(&candidate))?,
                    serde_json::to_string(&conflicts)?,
                    state
                ],
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            span.event(
                "failed",
                Some("preparation failed; abort or inspect transaction"),
            );
            self.conn()?
                .execute("UPDATE activations SET state='FAILED' WHERE id=?1", [&id])?;
            return Err(e);
        }
        Ok(id)
    }
    fn same(&self, a: &Option<Entry>, b: &Option<Entry>) -> Result<bool> {
        match (a, b) {
            (None, None) => Ok(true),
            (Some(a), Some(b)) => Ok(a.kind == b.kind
                && a.mode == b.mode
                && (a.content == b.content || self.bytes(a)? == self.bytes(b)?)),
            _ => Ok(false),
        }
    }
    pub fn conflict_list(&self, id: &str) -> Result<Vec<Conflict>> {
        let s: String =
            self.conn()?
                .query_row("SELECT conflicts FROM activations WHERE id=?1", [id], |r| {
                    r.get(0)
                })?;
        Ok(serde_json::from_str(&s)?)
    }
    pub fn resolve_conflict(&self, id: &str, mount: &str, path: &str, discard: bool) -> Result<()> {
        let _g = self.workspace_lock(&format!("tx-{id}"))?;
        let mut conflicts = self.conflict_list(id)?;
        let conflict = conflicts
            .iter_mut()
            .find(|c| c.mount == mount && c.path == path)
            .ok_or_else(|| error("conflict not found"))?;
        if (conflict.reason.contains("removed")
            || conflict.reason.contains("shadowed")
            || conflict.reason.contains("identity"))
            && !discard
        {
            return Err(error(
                "ownership conflict requires --discard (original edit retained in checkpoint)",
            ));
        }
        if discard {
            let (m, mut o) = self.view_state(&View::Transaction(id.into()))?;
            o.remove(&(mount.into(), path.into()));
            let _ = m;
            self.conn()?.execute(
                "UPDATE activations SET candidate=?2 WHERE id=?1",
                params![id, serde_json::to_string(&deltas(&o))?],
            )?;
        }
        conflict.resolved = true;
        let state = if conflicts.iter().all(|c| c.resolved) {
            "READY"
        } else {
            "CONFLICTED"
        };
        self.conn()?.execute(
            "UPDATE activations SET conflicts=?2,state=?3 WHERE id=?1",
            params![id, serde_json::to_string(&conflicts)?, state],
        )?;
        Ok(())
    }
    pub fn finish_activation(&self, id: &str, abort: bool) -> Result<()> {
        let c = self.conn()?;
        let (name, source, state, m, d): (String, i64, String, String, String) = c.query_row(
            "SELECT workspace,source,state,proposal,candidate FROM activations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let _g = self.workspace_lock(&name)?;
        let _txg = self.workspace_lock(&format!("tx-{id}"))?;
        if state == "COMMITTED" || state == "ABORTED" {
            return Err(error("transaction already finished"));
        }
        if !abort && (state != "READY" || self.head(&name)? != source) {
            return Err(error("transaction not ready or source advanced"));
        }
        let mut c = self.conn()?;
        let t = c.transaction()?;
        if !abort {
            t.execute(
                "INSERT INTO revisions(map,delta) VALUES(?1,?2)",
                params![m, d],
            )?;
            t.execute(
                "UPDATE workspaces SET head=?2,dirty=1,changed=unixepoch() WHERE name=?1",
                params![name, t.last_insert_rowid()],
            )?;
        }
        t.execute(
            "UPDATE workspaces SET activation=NULL WHERE name=?1 AND activation=?2",
            params![name, id],
        )?;
        t.execute(
            "UPDATE activations SET state=?2 WHERE id=?1",
            params![id, if abort { "ABORTED" } else { "COMMITTED" }],
        )?;
        t.commit()?;
        self.snapshot_unlocked(&name)?;
        if !abort {
            self.record_layout(&name)?;
        }
        Ok(())
    }
    pub fn record_layout(&self, name: &str) -> Result<()> {
        let _l = Lock::take(&self.root.join("layout.lock"))?;
        let path = self.root.join("layouts");
        std::fs::create_dir_all(&path)?;
        if !path.join(".git").exists() {
            git(&path, &["init"])?;
        }
        let (m, _) = self.state(self.head(name)?)?;
        atomic(
            &path.join("VERSION_MAP"),
            toml::to_string_pretty(&m)?.as_bytes(),
        )?;
        git(&path, &["add", "VERSION_MAP"])?;
        git(
            &path,
            &[
                "-c",
                "user.name=Gitalator",
                "-c",
                "user.email=gtl@localhost",
                "commit",
                "--allow-empty",
                "-m",
                &format!("Activate {name}"),
            ],
        )?;
        git(&path, &["branch", "-f", name, "HEAD"])?;
        Ok(())
    }
    pub fn export(&self, name: &str, destination: &Path) -> Result<serde_json::Value> {
        self.snapshot(name)?;
        let revision = self.head(name)?;
        let (map, overlay) = self.state(revision)?;
        if destination.exists() {
            return Err(error("export destination must not exist"));
        }
        std::fs::create_dir_all(destination)?;
        let dest = std::fs::canonicalize(destination)?;
        let mut repos = vec![];
        let mut used = std::collections::BTreeSet::new();
        for m in &map.mount {
            let edits: Vec<_> = overlay.iter().filter(|((id, _), _)| id == &m.id).collect();
            if edits.is_empty() {
                continue;
            }
            if !used.insert(&m.repo) {
                return Err(error(
                    "multiple edited mounts of one repository require separate exports",
                ));
            }
            let path = dest.join(&m.id);
            std::fs::create_dir_all(&path)?;
            git(
                &path,
                &[
                    "clone",
                    "--no-checkout",
                    "--no-hardlinks",
                    self.repo_path(&m.repo).to_str().unwrap(),
                    ".",
                ],
            )?;
            git(&path, &["checkout", "--detach", m.commit.as_ref().unwrap()])?;
            for ((_, p), e) in edits {
                let visible = join(&m.path, p);
                if !map
                    .owner(&visible)
                    .is_some_and(|(owner, _)| owner.id == m.id)
                {
                    continue;
                }
                let rel = join(&m.subtree, p);
                let file = path.join(&rel);
                safe_parents(&path, &file)?;
                match e {
                    None => {
                        if file.symlink_metadata().is_ok() {
                            if file.is_dir() && !file.is_symlink() {
                                std::fs::remove_dir_all(&file)?;
                            } else {
                                std::fs::remove_file(&file)?;
                            }
                        }
                    }
                    Some(e) => {
                        if e.kind == Kind::Directory {
                            std::fs::create_dir_all(&file)?;
                            continue;
                        }
                        if let Some(parent) = file.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        if file.symlink_metadata().is_ok() {
                            std::fs::remove_file(&file)?;
                        }
                        let b = self.bytes(e)?;
                        if e.kind == Kind::Symlink {
                            std::os::unix::fs::symlink(std::str::from_utf8(&b)?, &file)?;
                        } else {
                            atomic(&file, &b)?;
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(
                                &file,
                                std::fs::Permissions::from_mode(e.mode),
                            )?;
                        }
                    }
                }
            }
            git(&path, &["add", "-A"])?;
            git(
                &path,
                &[
                    "-c",
                    "user.name=Gitalator",
                    "-c",
                    "user.email=gtl@localhost",
                    "commit",
                    "--allow-empty",
                    "-m",
                    &format!("Gitalator workspace {name} revision {revision}"),
                ],
            )?;
            let head = String::from_utf8(git(&path, &["rev-parse", "HEAD"])?)?
                .trim()
                .to_string();
            repos.push(serde_json::json!({"mount":m.id,"repo":m.repo,"base":m.commit,"head":head,"path":path}));
        }
        let manifest = serde_json::json!({"format":1,"workspace":name,"revision":revision,"repositories":repos});
        atomic(
            &dest.join("gitalator-export.json"),
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        )?;
        self.conn()?.execute(
            "INSERT INTO exports(workspace,revision,path,manifest) VALUES(?1,?2,?3,?4)",
            params![name, revision, dest.to_string_lossy(), manifest.to_string()],
        )?;
        Ok(manifest)
    }
}
fn safe_parents(root: &Path, p: &Path) -> Result<()> {
    let rel = p.strip_prefix(root)?;
    let mut cur = root.to_path_buf();
    for part in rel.parent().unwrap_or(Path::new("")).components() {
        cur.push(part);
        if cur.is_symlink() {
            return Err(error("export path traverses symlink"));
        }
    }
    Ok(())
}
impl Store {
    pub fn gc(&self, dry_run: bool, max_bytes: u64) -> Result<serde_json::Value> {
        let _lock = Lock::take(&self.root.join("gc.lock"))?;
        let c = self.conn()?;
        let mut q = c.prepare("SELECT DISTINCT content,size FROM objects ORDER BY accessed")?;
        let rows = q
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut pins = std::collections::BTreeSet::new();
        let mut revisions =
            c.prepare("SELECT delta FROM revisions UNION ALL SELECT candidate FROM activations")?;
        for row in revisions.query_map([], |r| r.get::<_, String>(0))? {
            for d in serde_json::from_str::<Vec<Delta>>(&row?)? {
                if let Some(e) = d.entry {
                    pins.insert(e.content);
                }
            }
        }
        let mut total: u64 = rows.iter().map(|r| r.1).sum();
        let before = total;
        let mut evict = vec![];
        for (id, size) in rows {
            if total <= max_bytes {
                break;
            }
            if pins.contains(&id) {
                continue;
            }
            evict.push(id.clone());
            total = total.saturating_sub(size);
            if !dry_run {
                let path = self.fetched.root.join("objects").join(&id);
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
                c.execute("DELETE FROM objects WHERE content=?1", [id])?;
            }
        }
        Ok(
            serde_json::json!({"dry_run":dry_run,"cached_bytes_before":before,"cached_bytes_after":total,"evicted":evict,"history_retained":true}),
        )
    }
}
