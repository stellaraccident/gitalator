//! Planned layout changes. Fetch/validate before activation; never silently
//! replace an existing mount or its pin.
use crate::{
    activity,
    cli::Submodules,
    error,
    model::*,
    store::{json_write, token, Record, Store, Workspace},
    util::atomic,
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(crate) struct ImportOptions {
    pub(crate) skip_unavailable: bool,
    pub(crate) skipped: Vec<Value>,
}
impl ImportOptions {
    fn report(self, mut result: Value) -> Value {
        result["complete"] = json!(self.skipped.is_empty());
        result["skipped"] = json!(self.skipped);
        result
    }
}

pub struct Discovery {
    pub state: PathBuf,
    pub name: String,
    pub id: String,
}
pub fn discover(cwd: &Path) -> Result<Discovery> {
    let mut found = None;
    for directory in cwd.ancestors() {
        let Ok(depot) = fs::read_link(directory.join(".depotgit")) else {
            continue;
        };
        if !depot.is_absolute() {
            continue;
        }
        let Some(workspace) = depot.parent() else {
            continue;
        };
        let Ok(bytes) = fs::read(workspace.join("workspace.json")) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<Record>(&bytes) else {
            continue;
        };
        let Some(state) = workspace.parent().and_then(Path::parent) else {
            continue;
        };
        if state.join("workspaces").join(&record.id).join("depot") != depot {
            continue;
        }
        // The outer workspace wins over a similarly named file in component content.
        found = Some(Discovery {
            state: state.to_path_buf(),
            name: record.name,
            id: record.id,
        });
    }
    found.ok_or_else(|| {
        error("not inside a composed workspace; specify --workspace NAME and --state-dir")
    })
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn add_mount(
        &self,
        name: &str,
        expected_id: Option<&str>,
        url: &str,
        at: &str,
        revision: &str,
        submodules: Submodules,
    ) -> Result<Value> {
        self.add_mount_with_options(name, expected_id, url, at, revision, submodules, false)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn add_mount_with_options(
        &self,
        name: &str,
        expected_id: Option<&str>,
        url: &str,
        at: &str,
        revision: &str,
        submodules: Submodules,
        skip_unavailable: bool,
    ) -> Result<Value> {
        let at = if at == "." { "" } else { at };
        if !valid_path(at) {
            return Err(error("--at must be workspace-relative; use . for the root"));
        }
        let workspace = self.get(name)?;
        let (original, generation) = {
            let w = activity::lock(&workspace)?;
            w.quiescent()?;
            w.validate()?;
            if expected_id.is_some_and(|id| id != w.record.id) {
                return Err(error("workspace identity changed"));
            }
            if w.record.map.mounts.iter().any(|m| m.path == at) {
                return Err(error("a mount already occupies this path"));
            }
            if w.record.map.owner(at).is_some()
                && w.namespace
                    .lookup(at)?
                    .is_some_and(|b| b.kind == Kind::Directory)
                && !w.namespace.children(at)?.is_empty()
            {
                return Err(error("destination already contains mounted sources; replacement requires a resolve workflow"));
            }
            (w.record.map.clone(), w.record.generation.clone())
        };
        let mut map = original.clone();
        let mut options = ImportOptions {
            skip_unavailable,
            ..Default::default()
        };
        self.import_forest(
            &mut map,
            url,
            revision,
            at,
            "",
            submodules,
            false,
            &mut options,
        )?;
        Ok(options.report(self.finish_layout(name, &original, &generation, map)?))
    }

    fn layout_snapshot(&self, name: &str, expected_id: Option<&str>) -> Result<(Map, String)> {
        let workspace = self.get(name)?;
        let w = activity::lock(&workspace)?;
        w.quiescent()?;
        w.validate()?;
        if expected_id.is_some_and(|id| id != w.record.id) {
            return Err(error("workspace identity changed"));
        }
        Ok((w.record.map.clone(), w.record.generation.clone()))
    }

    pub fn remove_mount(
        &self,
        name: &str,
        expected_id: Option<&str>,
        at: &str,
        recursive: bool,
    ) -> Result<Value> {
        let at = mount_path(at)?;
        let (original, generation) = self.layout_snapshot(name, expected_id)?;
        if !original.mounts.iter().any(|m| m.path == at) {
            return Err(error("no mount at the requested path"));
        }
        let mut map = original.clone();
        map.mounts
            .retain(|m| m.path != at && !(recursive && under(&m.path, at)));
        // Removing a child may expose its parent's gitlink as an empty directory.
        // Preserve independent descendants; never cascade implicitly.
        for removed in original
            .mounts
            .iter()
            .filter(|m| !map.mounts.iter().any(|n| n.path == m.path))
            .cloned()
            .collect::<Vec<_>>()
        {
            if let Some(owner) = map.owner(&removed.path).map(|m| m.id.clone()) {
                map.mounts
                    .iter_mut()
                    .find(|m| m.id == owner)
                    .unwrap()
                    .uninitialized_submodules = true;
            }
        }
        map.repositories
            .retain(|id, _| map.mounts.iter().any(|m| &m.repository == id));
        self.finish_layout(name, &original, &generation, map)
    }

    pub fn expand_submodules(
        &self,
        name: &str,
        expected_id: Option<&str>,
        at: &str,
        depth: Submodules,
    ) -> Result<Value> {
        self.expand_submodules_with_options(name, expected_id, at, depth, false)
    }
    pub fn expand_submodules_with_options(
        &self,
        name: &str,
        expected_id: Option<&str>,
        at: &str,
        depth: Submodules,
        skip_unavailable: bool,
    ) -> Result<Value> {
        let at = mount_path(at)?;
        if depth == Submodules::None {
            return Err(error("expansion depth must be first-level or recursive"));
        }
        let (original, generation) = self.layout_snapshot(name, expected_id)?;
        let mut map = original.clone();
        let index = map
            .mounts
            .iter()
            .position(|m| m.path == at)
            .ok_or("no mount at the requested path")?;
        let mut options = ImportOptions {
            skip_unavailable,
            ..Default::default()
        };
        let mount = map.mounts[index].clone();
        let url = map.repositories[&mount.repository].url.clone();
        self.import_forest(
            &mut map,
            &url,
            &mount.commit,
            at,
            &mount.subtree,
            depth,
            true,
            &mut options,
        )?;
        Ok(options.report(self.finish_layout(name, &original, &generation, map)?))
    }

    fn finish_layout(
        &self,
        name: &str,
        original: &Map,
        generation: &str,
        map: Map,
    ) -> Result<Value> {
        map.validate()?;
        let changed = &map != original;
        let namespace = if changed {
            Some(self.prepare_namespace(&map)?)
        } else {
            None
        };
        let workspace = self.get(name)?;
        let mut w = activity::lock(&workspace)?;
        w.quiescent()?;
        w.validate()?;
        activity::check()?;
        if w.record.generation != generation
            || &w.record.map != original
            || !w.root.join("workspace.json").exists()
        {
            return Err(error(
                "workspace changed during layout preparation; retry against its current state",
            ));
        }
        let added: Vec<_> = map
            .mounts
            .iter()
            .filter(|m| !original.mounts.iter().any(|old| old.path == m.path))
            .cloned()
            .collect();
        let removed: Vec<_> = original
            .mounts
            .iter()
            .filter(|m| !map.mounts.iter().any(|new| new.path == m.path))
            .cloned()
            .collect();
        let updated: Vec<_> = map
            .mounts
            .iter()
            .filter(|m| {
                original
                    .mounts
                    .iter()
                    .any(|old| old.path == m.path && old != *m)
            })
            .cloned()
            .collect();
        if changed {
            let mut record = w.record.clone();
            record.map = map.clone();
            let candidate = Workspace {
                root: w.root.clone(),
                record,
                namespace: namespace.unwrap(),
                metadata: w.metadata.clone(),
                handles: 0,
                listed: Default::default(),
                disabled_batches: Default::default(),
            };
            candidate.validate_content().map_err(|e| error(format!("layout conflicts with depot overlay/tombstones: {e}; no layout changes were activated")))?;
            w.activate_layout(map, candidate.namespace)?;
        }
        Ok(
            json!({"workspace":name,"added":added,"removed":removed,"updated":updated,"changed":changed,"manifest":w.depot().join("depot.toml"),"staged":false,"state_dir":self.root}),
        )
    }
}

fn mount_path(at: &str) -> Result<&str> {
    let at = if at == "." { "" } else { at };
    if !valid_path(at) {
        return Err(error("--at must be workspace-relative; use . for the root"));
    }
    Ok(at)
}

/// Git resolves ./ and ../ submodule URLs relative to the repository URL itself.
pub(crate) fn resolve_url(parent: &str, child: &str) -> Result<String> {
    if !child.starts_with("./") && !child.starts_with("../") {
        return Ok(child.into());
    }
    let (prefix, path) = if let Some((scheme, tail)) = parent.split_once("://") {
        let (host, path) = tail.split_once('/').unwrap_or((tail, ""));
        (format!("{scheme}://{host}/"), path)
    } else if let Some((host, path)) = parent.split_once(':') {
        (format!("{host}:"), path)
    } else if let Some(path) = parent.strip_prefix('/') {
        ("/".into(), path)
    } else {
        return Err(error("relative parent repository URL is unsupported"));
    };
    let mut parts: Vec<_> = path.split('/').filter(|p| !p.is_empty()).collect();
    for part in child.split('/') {
        match part {
            "" | "." => (),
            ".." => {
                parts.pop().ok_or("submodule URL escapes its root")?;
            }
            _ => parts.push(part),
        }
    }
    Ok(prefix + &parts.join("/"))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutJournal {
    map: Map,
    generation: String,
}
impl Workspace {
    pub(crate) fn recover_layout(&mut self) -> Result<()> {
        let journal = self.root.join("layout.json");
        if !journal.exists() {
            return Ok(());
        }
        let change: LayoutJournal = serde_json::from_slice(&fs::read(&journal)?)?;
        change.map.validate()?;
        atomic(
            &self.depot().join("depot.toml"),
            toml::to_string(&change.map)?.as_bytes(),
        )?;
        self.record.map = change.map;
        self.record.generation = change.generation;
        self.save()?;
        fs::remove_file(journal)?;
        fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    fn activate_layout(
        &mut self,
        map: Map,
        namespace: std::sync::Arc<crate::namespace::Namespace>,
    ) -> Result<()> {
        activity::check()?;
        let _critical = activity::critical();
        json_write(
            &self.root.join("layout.json"),
            &LayoutJournal {
                map,
                generation: token(),
            },
        )?;
        crate::failpoint("layout-published");
        // Redo is idempotent. The on-disk index/HEAD and unrelated staged files
        // are never changed by a layout operation.
        let journal = self.root.join("layout.json");
        let change: LayoutJournal = serde_json::from_slice(&fs::read(&journal)?)?;
        atomic(
            &self.depot().join("depot.toml"),
            toml::to_string(&change.map)?.as_bytes(),
        )?;
        self.record.map = change.map;
        self.record.generation = change.generation;
        self.save()?;
        self.namespace = namespace;
        self.listed.lock().unwrap().clear();
        self.disabled_batches.lock().unwrap().clear();
        crate::failpoint("layout-applied");
        fs::remove_file(journal)?;
        fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_submodule_origins() {
        assert_eq!(
            resolve_url("https://github.com/ROCm/TheRock.git", "../half.git").unwrap(),
            "https://github.com/ROCm/half.git"
        );
        assert_eq!(
            resolve_url("git@github.com:ROCm/TheRock.git", "../half.git").unwrap(),
            "git@github.com:ROCm/half.git"
        );
        assert_eq!(
            resolve_url("/tmp/repos/main", "../child").unwrap(),
            "/tmp/repos/child"
        );
        assert_eq!(
            resolve_url("file:///tmp/repos/main", "../child").unwrap(),
            "file:///tmp/repos/child"
        );
    }
}
