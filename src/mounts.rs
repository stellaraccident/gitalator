//! Planned layout changes. Fetch/validate before activation; never silently
//! replace an existing mount or its pin.
use crate::{
    activity,
    cli::Submodules,
    error, git,
    model::*,
    store::{directory_index, json_write, token, Record, Store, Workspace},
    util::{atomic, hash},
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Default)]
struct ImportOptions {
    skip_unavailable: bool,
    skipped: Vec<Value>,
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
            if w.record.map.owner(at).is_some() && w.base.keys().any(|p| p != at && under(p, at)) {
                return Err(error("destination already contains mounted sources; replacement requires a resolve workflow"));
            }
            (w.record.map.clone(), w.record.generation.clone())
        };
        let mut map = original.clone();
        let mut options = ImportOptions {
            skip_unavailable,
            ..Default::default()
        };
        self.import_mount(
            &mut map,
            url,
            revision,
            at,
            submodules,
            &mut BTreeSet::new(),
            0,
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
        self.expand_mount(
            &mut map,
            index,
            depth,
            &mut BTreeSet::new(),
            0,
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
        let base = if changed {
            self.load_base(&map)?
        } else {
            BTreeMap::new()
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
                base,
                directories: Default::default(),
                metadata: w.metadata.clone(),
                handles: 0,
                listed: Default::default(),
                disabled_batches: Default::default(),
            };
            candidate.validate_content().map_err(|e| error(format!("layout conflicts with depot overlay/tombstones: {e}; no layout changes were activated")))?;
            w.activate_layout(map, candidate.base)?;
        }
        Ok(
            json!({"workspace":name,"added":added,"removed":removed,"updated":updated,"changed":changed,"manifest":w.depot().join("depot.toml"),"staged":false,"state_dir":self.root}),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn import_mount(
        &self,
        map: &mut Map,
        url: &str,
        revision: &str,
        path: &str,
        mode: Submodules,
        stack: &mut BTreeSet<(String, String)>,
        depth: usize,
        reuse: bool,
        options: &mut ImportOptions,
    ) -> Result<()> {
        activity::check()?;
        if depth > 32 || map.mounts.len() >= 4096 {
            return Err(error("submodule import exceeds depth/mount limit"));
        }
        if map.mounts.iter().any(|m| m.path == path) {
            return Err(error(format!("mount path already exists: {path}")));
        }
        let key = map
            .repositories
            .iter()
            .find(|(_, r)| r.url == url)
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| format!("repo-{}", &hash(url.as_bytes())[..16]));
        if map.repositories.get(&key).is_some_and(|r| r.url != url) {
            return Err(error("repository ID collision"));
        }
        map.repositories
            .insert(key.clone(), Repository { url: url.into() });
        map.validate()?; // Validate remote syntax before contacting it.
        let repo = self.repo(url)?;
        let commit = self.fetch(&repo, revision)?;
        let id = if path.is_empty() {
            "root".into()
        } else {
            format!("mount-{}", &hash(path.as_bytes())[..16])
        };
        map.mounts.push(Mount {
            id,
            repository: key,
            commit: commit.clone(),
            path: path.into(),
            subtree: String::new(),
            uninitialized_submodules: mode == Submodules::None,
        });
        map.validate()?;
        activity::event(
            "info",
            &format!(
                "Import mount {} at {commit}; submodules {mode:?}",
                if path.is_empty() { "/" } else { path }
            ),
        );
        self.expand_mount(
            map,
            map.mounts.len() - 1,
            mode,
            stack,
            depth,
            reuse,
            options,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn expand_mount(
        &self,
        map: &mut Map,
        index: usize,
        mode: Submodules,
        stack: &mut BTreeSet<(String, String)>,
        depth: usize,
        reuse: bool,
        options: &mut ImportOptions,
    ) -> Result<()> {
        activity::check()?;
        if mode == Submodules::None {
            return Ok(());
        }
        if depth > 32 {
            return Err(error("submodule import exceeds depth/mount limit"));
        }
        let mount = map.mounts[index].clone();
        let path = mount.path.as_str();
        let commit = mount.commit.clone();
        let url = map.repositories[&mount.repository].url.clone();
        if !stack.insert((url.clone(), commit.clone())) {
            return Err(error("recursive submodule cycle"));
        }
        let repo = self.repo(&url)?;
        self.fetch(&repo, &commit)?;
        activity::event(
            "info",
            &format!(
                "Expand submodules at {} ({commit}): {mode:?}",
                if path.is_empty() { "/" } else { path }
            ),
        );
        let mut missing_children = false;
        {
            let bare = self.root.join("repos").join(repo);
            let tree_spec = if mount.subtree.is_empty() {
                commit.clone()
            } else {
                format!("{commit}:{}", mount.subtree)
            };
            let tree = git::run(&bare, &["ls-tree", "-r", "-z", &tree_spec])?;
            let mut links = vec![];
            for entry in tree.split(|b| *b == 0).filter(|e| !e.is_empty()) {
                activity::check()?;
                let (meta, name) = std::str::from_utf8(entry)?
                    .split_once('\t')
                    .ok_or("invalid tree entry")?;
                let fields: Vec<_> = meta.split_whitespace().collect();
                if fields[0] == "160000" {
                    links.push((name.to_owned(), fields[2].to_owned()));
                }
            }
            if !links.is_empty() {
                let config = format!("{commit}:.gitmodules");
                let pairs = git::run(
                    &bare,
                    &[
                        "config",
                        "--null",
                        "--blob",
                        &config,
                        "--get-regexp",
                        r"^submodule\..*\.path$",
                    ],
                )?;
                let mut names = BTreeMap::new();
                for pair in pairs.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                    let (key, path) = std::str::from_utf8(pair)?
                        .split_once('\n')
                        .ok_or("invalid submodule config")?;
                    if names
                        .insert(
                            path.to_string(),
                            key.strip_suffix(".path")
                                .ok_or("invalid submodule key")?
                                .to_string(),
                        )
                        .is_some()
                    {
                        return Err(error("duplicate submodule path"));
                    }
                }
                for (child, pin) in links {
                    if !valid_path(&child) {
                        return Err(error("invalid submodule path"));
                    }
                    let key = names
                        .get(&join(&mount.subtree, &child))
                        .ok_or("gitlink missing from .gitmodules")?;
                    let child_url = git::text(
                        &bare,
                        &["config", "--blob", &config, "--get", &format!("{key}.url")],
                    )?;
                    let child_url = resolve_url(&url, &child_url)?;
                    let child_path = join(path, &child);
                    let child_mode = if mode == Submodules::Recursive {
                        Submodules::Recursive
                    } else {
                        Submodules::None
                    };
                    // Roll back only this child's preparation on an access failure.
                    // Successfully expanded siblings and ancestors stay in the candidate.
                    let saved = options
                        .skip_unavailable
                        .then(|| (map.clone(), stack.clone(), options.skipped.len()));
                    let result = if let Some(existing) =
                        map.mounts.iter().position(|m| m.path == child_path)
                    {
                        let m = &map.mounts[existing];
                        if !reuse
                            || map.repositories[&m.repository].url != child_url
                            || m.commit != pin
                            || !m.subtree.is_empty()
                        {
                            return Err(error(format!("submodule mount conflict at {child_path}: existing origin/pin/subtree differs or path is occupied; use an explicit resolve workflow (no mounts changed)")));
                        }
                        self.expand_mount(
                            map,
                            existing,
                            child_mode,
                            stack,
                            depth + 1,
                            reuse,
                            options,
                        )
                    } else {
                        self.import_mount(
                            map,
                            &child_url,
                            &pin,
                            &child_path,
                            child_mode,
                            stack,
                            depth + 1,
                            reuse,
                            options,
                        )
                    };
                    if let Err(e) = result {
                        activity::check()?; // Cancellation always aborts the whole plan.
                        let unavailable = e
                            .downcast_ref::<git::CommandFailure>()
                            .is_some_and(|e| e.command == "fetch");
                        if !options.skip_unavailable || !unavailable {
                            return Err(e);
                        }
                        let (saved_map, saved_stack, skipped_len) = saved.unwrap();
                        *map = saved_map;
                        *stack = saved_stack;
                        options.skipped.truncate(skipped_len);
                        missing_children |= !map.mounts.iter().any(|m| m.path == child_path);
                        let failure = e.downcast_ref::<git::CommandFailure>().unwrap();
                        let message = format!("git {}: {}", failure.command, failure.message);
                        activity::event(
                            "warn",
                            &format!("Skipped submodule {child_path}: {message}"),
                        );
                        if let Some(command) = &failure.retry_command {
                            activity::event(
                                "warn",
                                &format!("Interactive retry for {child_path}: {command}"),
                            );
                        }
                        options.skipped.push(json!({"path":child_path,"url":git::public_origin(&child_url),"commit":pin,"error":message,"retry_command":failure.retry_command}));
                    }
                }
            }
        }
        map.mounts[index].uninitialized_submodules = missing_children;
        stack.remove(&(url, commit));
        Ok(())
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
fn resolve_url(parent: &str, child: &str) -> Result<String> {
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
    fn activate_layout(&mut self, map: Map, base: BTreeMap<String, Base>) -> Result<()> {
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
        self.directories = directory_index(&base);
        self.base = base;
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
