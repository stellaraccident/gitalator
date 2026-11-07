use crate::{backend::git, cache::hash, db::Store, error, model::*, Result};
use rusqlite::params;
use std::{collections::BTreeSet, path::PathBuf};

pub fn default_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".gitalator")
}
/// Conservative normalization: do not merge transports, SSH aliases, or URL paths.
pub fn canonical_remote(remote: &str) -> Result<String> {
    if remote.is_empty() || remote.starts_with('-') || remote.chars().any(char::is_control) {
        return Err(error("invalid repository origin"));
    }
    if let Some(path) = remote.strip_prefix("file://") {
        if !path.starts_with('/') {
            return Err(error("file URLs must have an absolute local path"));
        }
        return Ok(format!("file://{}", std::fs::canonicalize(path)?.display()));
    }
    if let Some((scheme, rest)) = remote.split_once("://") {
        let (authority, path) = rest
            .split_once('/')
            .ok_or_else(|| error("remote URL has no repository path"))?;
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return Err(error("unsupported repository URL scheme"));
        }
        if remote.contains('?')
            || remote.contains('#')
            || (scheme != "ssh" && authority.contains('@'))
        {
            return Err(error("use Git credential helpers, not credentials or query parameters in repository URLs"));
        }
        let authority = if let Some((user, host)) = authority.rsplit_once('@') {
            format!("{user}@{}", host.to_ascii_lowercase())
        } else {
            authority.to_ascii_lowercase()
        };
        let path = path.trim_end_matches('/');
        let path = if authority == "github.com" || authority == "git@github.com" {
            path.strip_suffix(".git").unwrap_or(path)
        } else {
            path
        };
        return Ok(format!("{scheme}://{authority}/{path}"));
    }
    if remote.contains(':') && !remote.starts_with('/') && !remote.starts_with('.') {
        return Ok(remote.trim_end_matches('/').into());
    }
    Ok(format!(
        "file://{}",
        std::fs::canonicalize(remote)?.display()
    ))
}
fn relative_remote(parent: &str, child: &str) -> Result<String> {
    if !child.starts_with("./") && !child.starts_with("../") {
        return canonical_remote(child);
    }
    // Git resolves relative submodule URLs relative to the repository URL itself.
    let (prefix, path) = if let Some((scheme, rest)) = parent.split_once("://") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        (format!("{scheme}://{host}/"), path)
    } else if let Some((host, path)) = parent.split_once(':') {
        (format!("{host}:"), path)
    } else {
        return Err(error(
            "relative submodule URL requires a canonical parent origin",
        ));
    };
    let mut components: Vec<_> = path.split('/').filter(|s| !s.is_empty()).collect();
    for part in child.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(error("submodule URL escapes origin root"));
                }
            }
            p => components.push(p),
        }
    }
    canonical_remote(&format!("{prefix}{}", components.join("/")))
}
impl Store {
    pub fn register_remote(&self, remote: &str) -> Result<String> {
        let origin = canonical_remote(remote)?;
        let id = format!("repo-{}", hash(origin.as_bytes()));
        self.repo_add(&id, &origin)?;
        Ok(id)
    }
    fn default_revision(&self, repo: &str) -> Result<String> {
        let origin: String =
            self.conn()?
                .query_row("SELECT origin FROM repos WHERE id=?1", [repo], |r| r.get(0))?;
        let text = String::from_utf8(git(
            &self.root,
            &["ls-remote", "--symref", &origin, "HEAD"],
        )?)?;
        text.lines()
            .find_map(|line| {
                line.strip_prefix("ref: ")
                    .and_then(|l| l.split_once('\t'))
                    .map(|(r, _)| r.to_string())
            })
            .ok_or_else(|| error("remote has no default branch; specify --revision"))
    }
    fn submodule_mounts(&self, parent: &Mount) -> Result<Vec<Mount>> {
        let commit = parent
            .commit
            .as_deref()
            .ok_or_else(|| error("unresolved parent mount"))?;
        let repo = self.repo_path(&parent.repo)?;
        let spec = format!("{commit}:.gitmodules");
        let tree = git(&repo, &["ls-tree", "-z", commit, "--", ".gitmodules"])?;
        if tree.is_empty() {
            return Ok(vec![]);
        }
        let modules = git(&repo, &["config", "-z", "--blob", &spec, "--list"])?;
        let mut paths = std::collections::BTreeMap::new();
        for record in modules.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let record = std::str::from_utf8(record)?;
            let (key, value) = record
                .split_once('\n')
                .ok_or_else(|| error("invalid .gitmodules record"))?;
            if key.starts_with("submodule.") {
                paths.insert(key.to_string(), value.to_string());
            }
        }
        let origin: String = self.conn()?.query_row(
            "SELECT origin FROM repos WHERE id=?1",
            [&parent.repo],
            |r| r.get(0),
        )?;
        let mut result = vec![];
        for (key, path) in &paths {
            let Some(base) = key.strip_suffix(".path") else {
                continue;
            };
            if path.is_empty() || !path_ok(path) {
                return Err(error("invalid submodule path"));
            }
            let url = paths
                .get(&format!("{base}.url"))
                .ok_or_else(|| error("submodule missing URL"))?;
            let raw = git(&repo, &["ls-tree", "-z", commit, "--", path])?;
            let raw = std::str::from_utf8(&raw)?;
            let (header, actual) = raw
                .trim_end_matches('\0')
                .split_once('\t')
                .ok_or_else(|| error("submodule has no committed gitlink"))?;
            let fields: Vec<_> = header.split_whitespace().collect();
            if fields.len() != 3 || fields[0] != "160000" || fields[1] != "commit" || actual != path
            {
                return Err(error("submodule path is not a committed gitlink"));
            }
            let remote = self.register_remote(&relative_remote(&origin, url)?)?;
            let full = join(&parent.path, path);
            result.push(Mount {
                id: format!("mount-{}", hash(full.as_bytes())),
                path: full,
                repo: remote,
                revision: fields[2].into(),
                commit: None,
                subtree: String::new(),
            });
        }
        Ok(result)
    }
    pub fn create_resolved(
        &self,
        name: &str,
        map: &Map,
        overlay: &Overlay,
        draft: &str,
    ) -> Result<()> {
        let _lock = self.workspace_lock(name)?;
        map.validate()?;
        let delta: Vec<_> = overlay
            .iter()
            .map(|((mount, path), entry)| Delta {
                mount: mount.clone(),
                path: path.clone(),
                entry: entry.clone(),
            })
            .collect();
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO revisions(parent,map,delta) VALUES(NULL,?1,?2)",
            params![serde_json::to_string(map)?, serde_json::to_string(&delta)?],
        )?;
        tx.execute(
            "INSERT INTO workspaces(name,head,draft) VALUES(?1,?2,?3)",
            params![name, tx.last_insert_rowid(), draft],
        )?;
        tx.commit()?;
        self.observe_committed()
    }
    pub fn new_workspace(
        &self,
        name: &str,
        from: Option<&str>,
        like: Option<&str>,
        clone: Option<&str>,
        mount_name: Option<&str>,
        revision: Option<&str>,
        no_submodules: bool,
    ) -> Result<()> {
        if [from, like, clone].iter().filter(|s| s.is_some()).count() != 1 {
            return Err(error("choose exactly one of --from, --like, --clone"));
        }
        if let Some(source) = like.or(clone) {
            if source == name {
                return Err(error("source and destination workspace are identical"));
            }
            // Capture under the source lock, then release before acquiring destination.
            let (map, overlay, draft) = {
                let _lock = self.workspace_lock(source)?;
                if clone.is_some() && self.frozen(source)? {
                    return Err(error("finish or abort source activation before cloning"));
                }
                let (map, overlay) = self.state(self.head(source)?)?;
                let draft = if clone.is_some() {
                    self.draft(source)?
                } else {
                    toml::to_string_pretty(&map)?
                };
                (
                    map,
                    if clone.is_some() {
                        overlay
                    } else {
                        Overlay::new()
                    },
                    draft,
                )
            };
            return self.create_resolved(name, &map, &overlay, &draft);
        }
        let remote = from.unwrap();
        let repo = self.register_remote(remote)?;
        let derived = remote
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .unwrap_or("sources")
            .trim_end_matches(".git");
        let top = mount_name.unwrap_or(derived);
        if top.is_empty() || !path_ok(top) || top.contains('/') {
            return Err(error(
                "top-level mount name must be a single path component",
            ));
        }
        let revision = revision
            .map(str::to_string)
            .map(Ok)
            .unwrap_or_else(|| self.default_revision(&repo))?;
        let root = Mount {
            id: "root".into(),
            path: top.into(),
            repo,
            revision,
            commit: None,
            subtree: String::new(),
        };
        let mut map = self.resolve_map(&Map {
            format: 1,
            mount: vec![root],
        })?;
        if !no_submodules {
            let children = self.submodule_mounts(&map.mount[0])?;
            if !children.is_empty() {
                map.mount.extend(
                    self.resolve_map(&Map {
                        format: 1,
                        mount: children,
                    })?
                    .mount,
                );
            }
        }
        self.create_resolved(name, &map, &Overlay::new(), &toml::to_string_pretty(&map)?)
    }
    pub fn expand_submodules(
        &self,
        name: &str,
        paths: &[String],
        recursive: bool,
    ) -> Result<usize> {
        let (head, mut map, overlay, draft) = {
            let _lock = self.workspace_lock(name)?;
            if self.frozen(name)? {
                return Err(error("finish or abort activation first"));
            }
            let head = self.head(name)?;
            let (map, overlay) = self.state(head)?;
            let draft = self.draft(name)?;
            let mut active = map.clone();
            for m in &mut active.mount {
                m.commit = None;
            }
            let mut proposed = Map::parse(&draft)?;
            for m in &mut proposed.mount {
                m.commit = None;
            }
            if serde_json::to_string(&active)? != serde_json::to_string(&proposed)? {
                return Err(error(
                    "activate or discard staged VERSION_MAP changes before expansion",
                ));
            }
            (head, map, overlay, draft)
        };
        for path in paths {
            if !map.mount.iter().any(|m| &m.path == path) {
                return Err(error(
                    "expansion path must name an existing repository mount",
                ));
            }
        }
        let mut pending: Vec<_> = map
            .mount
            .iter()
            .filter(|m| paths.is_empty() || paths.contains(&m.path))
            .map(|m| (m.clone(), BTreeSet::new()))
            .collect();
        let mut visited = BTreeSet::new();
        let mut added = 0;
        while let Some((parent, mut ancestors)) = pending.pop() {
            let identity = (self.repo_storage(&parent.repo)?, parent.commit.clone());
            if !ancestors.insert(identity) {
                return Err(error("recursive submodule cycle"));
            }
            if !visited.insert(parent.path.clone()) {
                continue;
            }
            if visited.len() > 4096 {
                return Err(error("submodule expansion exceeds 4096 mounts"));
            }
            for child in self.submodule_mounts(&parent)? {
                if let Some(existing) = map.mount.iter().find(|m| m.path == child.path) {
                    if self.repo_storage(&existing.repo)? != self.repo_storage(&child.repo)?
                        || existing.commit.as_ref() != Some(&child.revision)
                    {
                        return Err(error("existing mount conflicts with submodule pin"));
                    }
                    if recursive {
                        pending.push((existing.clone(), ancestors.clone()));
                    }
                    continue;
                }
                if overlay.iter().any(|((id, p), _)| {
                    map.mount.iter().find(|m| &m.id == id).is_some_and(|m| {
                        let p = join(&m.path, p);
                        p == child.path || p.starts_with(&(child.path.clone() + "/"))
                    })
                }) {
                    return Err(error("source edits overlap a new submodule mount"));
                }
                let resolved = self
                    .resolve_map(&Map {
                        format: 1,
                        mount: vec![child],
                    })?
                    .mount
                    .remove(0);
                if recursive {
                    pending.push((resolved.clone(), ancestors.clone()));
                }
                map.mount.push(resolved);
                added += 1;
            }
        }
        if added == 0 {
            return Ok(0);
        }
        map.validate()?;
        let _lock = self.workspace_lock(name)?;
        if self.head(name)? != head || self.draft(name)? != draft || self.frozen(name)? {
            return Err(error("workspace changed during expansion; retry"));
        }
        let deltas: Vec<_> = overlay
            .into_iter()
            .map(|((mount, path), entry)| Delta { mount, path, entry })
            .collect();
        // Publish map, overlay and draft together, without refreshing existing refs.
        let mut c = self.conn()?;
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO revisions(parent,map,delta) VALUES(NULL,?1,?2)",
            params![
                serde_json::to_string(&map)?,
                serde_json::to_string(&deltas)?
            ],
        )?;
        tx.execute(
            "UPDATE workspaces SET head=?2,draft=?3,dirty=1 WHERE name=?1",
            params![name, tx.last_insert_rowid(), toml::to_string_pretty(&map)?],
        )?;
        tx.commit()?;
        self.observe_committed()?;
        Ok(added)
    }
}
