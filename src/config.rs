use crate::{
    cache::{atomic, Lock},
    db::Store,
    error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
pub static BACKEND_SECONDS: AtomicU64 = AtomicU64::new(300);
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mount: Option<PathBuf>,
    pub snapshot_idle_seconds: u64,
    pub snapshot_max_seconds: u64,
    pub cache_bytes: u64,
    pub metadata_cache_bytes: u64,
    pub backend_timeout_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            mount: None,
            snapshot_idle_seconds: 2,
            snapshot_max_seconds: 30,
            cache_bytes: 1024 * 1024 * 1024,
            metadata_cache_bytes: 512 * 1024 * 1024,
            backend_timeout_seconds: 300,
        }
    }
}
impl Config {
    pub fn load(root: &Path) -> Result<Self> {
        let result = match std::fs::read_to_string(root.join("config.toml")) {
            Ok(text) => toml::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        result.validate()?;
        Ok(result)
    }
    pub fn mount_path(&self, root: &Path) -> PathBuf {
        match &self.mount {
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => root.join(p),
            None => {
                let default = crate::ergonomics::default_root();
                // Store paths are canonicalized at startup; HOME or the state
                // directory may itself be a symlink.
                let canonical = std::fs::canonicalize(&default).unwrap_or(default.clone());
                if root == default || root == canonical {
                    default.with_file_name(".gitalator")
                } else {
                    root.join("mount")
                }
            }
        }
    }
    pub fn validate(&self) -> Result<()> {
        if self.snapshot_idle_seconds == 0
            || self.snapshot_max_seconds < self.snapshot_idle_seconds
            || self.metadata_cache_bytes == 0
            || self.backend_timeout_seconds == 0
            || [
                self.snapshot_idle_seconds,
                self.snapshot_max_seconds,
                self.cache_bytes,
                self.metadata_cache_bytes,
                self.backend_timeout_seconds,
            ]
            .iter()
            .any(|v| *v > i64::MAX as u64)
        {
            return Err(error("invalid configuration: positive idle/metadata/deadline limits and max >= idle required; values must fit i64"));
        }
        if self.mount.as_ref().is_some_and(|p| {
            p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        }) {
            return Err(error("mount path must not contain .."));
        }
        Ok(())
    }
    pub fn apply(&self, s: &Store) -> Result<()> {
        self.validate()?;
        if let Some(info) = s.runtime.daemon.lock().unwrap().as_ref() {
            if info.configured_mount != self.mount_path(&s.root) {
                return Err(error("mount location change requires daemon restart"));
            }
        }
        let mut c = s.conn()?;
        let tx = c.transaction()?;
        for (key, value) in [
            ("snapshot_idle_seconds", self.snapshot_idle_seconds),
            ("snapshot_max_seconds", self.snapshot_max_seconds),
            ("cache_bytes", self.cache_bytes),
            ("metadata_cache_bytes", self.metadata_cache_bytes),
        ] {
            tx.execute(
                "INSERT OR REPLACE INTO settings VALUES(?1,?2)",
                rusqlite::params![key, value],
            )?;
        }
        tx.commit()?;
        s.runtime.limit(self.metadata_cache_bytes as usize);
        BACKEND_SECONDS.store(self.backend_timeout_seconds, Ordering::Release);
        Ok(())
    }
    pub fn update(s: &Store, edit: impl FnOnce(&mut Self)) -> Result<Self> {
        let _lock = Lock::take(&s.root.join("config.lock"))?;
        let old = Self::load(&s.root)?;
        let mut new = old.clone();
        edit(&mut new);
        new.validate()?;
        // Config is authoritative; roll it back if applying derived runtime settings fails.
        atomic(
            &s.root.join("config.toml"),
            toml::to_string_pretty(&new)?.as_bytes(),
        )?;
        if let Err(e) = new.apply(s) {
            atomic(
                &s.root.join("config.toml"),
                toml::to_string_pretty(&old)?.as_bytes(),
            )?;
            return Err(e);
        }
        Ok(new)
    }
}
