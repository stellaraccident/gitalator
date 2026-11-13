//! Additive layout changes. Fetch/validate before activation; never silently
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
        let before = map.mounts.len();
        self.import_mount(
            &mut map,
            url,
            revision,
            at,
            submodules,
            &mut BTreeSet::new(),
            0,
        )?;
        map.validate()?;
        let base = self.load_base(&map)?;
        let added = map.mounts[before..].to_vec();
        let mut w = activity::lock(&workspace)?;
        w.quiescent()?;
        w.validate()?;
        if w.record.generation != generation
            || w.record.map != original
            || !w.root.join("workspace.json").exists()
        {
            return Err(error(
                "workspace changed during import; retry against its current state",
            ));
        }
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
        candidate.validate_content()?;
        w.activate_layout(map, candidate.base)?;
        Ok(
            json!({"workspace":name,"added":added,"manifest":w.depot().join("depot.toml"),"staged":false,"state_dir":self.root}),
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
        if !stack.insert((url.into(), commit.clone())) {
            return Err(error("recursive submodule cycle"));
        }
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
        if mode != Submodules::None {
            let bare = self.root.join("repos").join(repo);
            let tree = git::run(&bare, &["ls-tree", "-r", "-z", &commit])?;
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
                        .get(&child)
                        .ok_or("gitlink missing from .gitmodules")?;
                    let child_url = git::text(
                        &bare,
                        &["config", "--blob", &config, "--get", &format!("{key}.url")],
                    )?;
                    let child_url = resolve_url(url, &child_url)?;
                    self.import_mount(
                        map,
                        &child_url,
                        &pin,
                        &join(path, &child),
                        if mode == Submodules::Recursive {
                            Submodules::Recursive
                        } else {
                            Submodules::None
                        },
                        stack,
                        depth + 1,
                    )?;
                }
            }
        }
        stack.remove(&(url.into(), commit));
        Ok(())
    }
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
        // are never changed by an additive layout operation.
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
