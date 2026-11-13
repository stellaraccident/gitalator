use crate::{error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Map {
    pub format: u32,
    #[serde(default)]
    pub repositories: BTreeMap<String, Repository>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub url: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub id: String,
    pub repository: String,
    pub commit: String,
    pub path: String,
    #[serde(default)]
    pub subtree: String,
    /// Deliberately leave gitlinks without explicit mounts as empty directories.
    #[serde(default, skip_serializing_if = "is_false")]
    pub uninitialized_submodules: bool,
}
pub const RESERVED: [&str; 5] = [
    ".depotgit",
    ".depotstate.json",
    ".mountgit",
    ".mountcommit",
    ".mountstate.json",
];
pub fn valid_path(p: &str) -> bool {
    p.is_empty()
        || (!p.starts_with('/')
            && !p.contains('\0')
            && p.split('/')
                .all(|c| !c.is_empty() && c != "." && c != ".." && c != ".git"))
}
pub fn name(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('.')
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
pub fn oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn under(path: &str, root: &str) -> bool {
    root.is_empty() || path == root || path.strip_prefix(root).is_some_and(|s| s.starts_with('/'))
}
pub fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.into()
    } else if b.is_empty() {
        a.into()
    } else {
        format!("{a}/{b}")
    }
}
impl Map {
    pub fn parse(s: &str) -> Result<Self> {
        let m: Self = toml::from_str(s)?;
        m.validate()?;
        Ok(m)
    }
    pub fn validate(&self) -> Result<()> {
        if self.format != 1 {
            return Err(error("unsupported depot format"));
        }
        let mut ids = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for (id, r) in &self.repositories {
            if !name(id)
                || r.url.is_empty()
                || r.url.starts_with('-')
                || r.url.contains('?')
                || r.url.contains('#')
                || r.url
                    .split_once("://")
                    .is_some_and(|(_, rest)| rest.split('/').next().unwrap_or("").contains('@'))
            {
                return Err(error("invalid repository"));
            }
        }
        for m in &self.mounts {
            if !name(&m.id)
                || !oid(&m.commit)
                || !valid_path(&m.path)
                || !valid_path(&m.subtree)
                || !self.repositories.contains_key(&m.repository)
                || !ids.insert(&m.id)
                || !paths.insert(&m.path)
                || m.path.split('/').any(|x| RESERVED.contains(&x))
            {
                return Err(error("invalid or duplicate mount"));
            }
        }
        Ok(())
    }
    pub fn owner(&self, p: &str) -> Option<&Mount> {
        self.mounts
            .iter()
            .filter(|m| under(p, &m.path))
            .max_by_key(|m| m.path.len())
    }
    pub fn reserved(&self, p: &str) -> bool {
        [".depotgit", ".depotstate.json"]
            .iter()
            .any(|r| under(p, r))
            || self.mounts.iter().any(|m| {
                [".mountgit", ".mountcommit", ".mountstate.json"]
                    .iter()
                    .any(|r| under(p, &join(&m.path, r)))
            })
    }
    pub fn protected(&self, p: &str) -> bool {
        p.is_empty() || self.mounts.iter().any(|m| under(&m.path, p)) || self.reserved(p)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub format: u32,
    pub kind: String,
    pub mount_id: String,
    pub base_commit: String,
    pub base_oid: String,
    pub base_kind: Kind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Base {
    pub commit_time: i64,
    pub kind: Kind,
    pub mode: u32,
    pub oid: String,
    pub repo: String,
    pub mount_id: String,
}

fn is_false(value: &bool) -> bool {
    !value
}
