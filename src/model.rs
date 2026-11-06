use crate::{error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Map {
    pub format: u32,
    #[serde(default)]
    pub mount: Vec<Mount>,
}
impl Default for Map {
    fn default() -> Self {
        Self {
            format: 1,
            mount: vec![],
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    pub id: String,
    pub path: String,
    pub repo: String,
    pub revision: String,
    #[serde(default)]
    pub subtree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub mode: u32,
    pub content: String,
    pub size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delta {
    pub mount: String,
    pub path: String,
    pub entry: Option<Entry>,
}
pub type Overlay = BTreeMap<(String, String), Option<Entry>>;
pub fn path_ok(p: &str) -> bool {
    !p.starts_with('/')
        && !p.contains('\0')
        && (p.is_empty() || p.split('/').all(|c| !c.is_empty() && c != "." && c != ".."))
}
pub fn name_ok(p: &str) -> bool {
    !p.is_empty()
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
impl Map {
    pub fn parse(s: &str) -> Result<Self> {
        let m: Self = toml::from_str(s)?;
        m.validate()?;
        Ok(m)
    }
    pub fn validate(&self) -> Result<()> {
        if self.format != 1 {
            return Err(error("unsupported VERSION_MAP format"));
        }
        let (mut ids, mut paths) = (BTreeSet::new(), BTreeSet::new());
        for m in &self.mount {
            if !name_ok(&m.id)
                || !name_ok(&m.repo)
                || !path_ok(&m.path)
                || !path_ok(&m.subtree)
                || m.revision.is_empty()
                || m.revision.starts_with('-')
            {
                return Err(error("invalid mount identity, path or revision"));
            }
            if !ids.insert(&m.id) || !paths.insert(&m.path) {
                return Err(error("duplicate mount ID or path"));
            }
        }
        Ok(())
    }
    pub fn owner(&self, path: &str) -> Option<(&Mount, String)> {
        self.mount
            .iter()
            .filter(|m| {
                m.path.is_empty() || path == m.path || path.starts_with(&(m.path.clone() + "/"))
            })
            .max_by_key(|m| m.path.len())
            .map(|m| {
                (
                    m,
                    if m.path.is_empty() {
                        path.to_string()
                    } else {
                        path.strip_prefix(&m.path)
                            .unwrap()
                            .trim_start_matches('/')
                            .to_string()
                    },
                )
            })
    }
    pub fn boundary(&self, path: &str) -> bool {
        self.mount
            .iter()
            .any(|m| m.path == path || m.path.starts_with(&(path.to_string() + "/")))
    }
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership() {
        let m=Map::parse("format=1\n[[mount]]\nid='a'\nrepo='a'\npath=''\nrevision='main'\n[[mount]]\nid='b'\nrepo='b'\npath='a/b'\nrevision='main'").unwrap();
        assert_eq!(m.owner("a/b/c").unwrap().0.id, "b");
        assert_eq!(m.owner("a/bb").unwrap().0.id, "a");
        assert!(!path_ok("a/../b"));
    }
}
