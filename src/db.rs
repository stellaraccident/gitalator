use crate::{
    cache::{Cache, Lock},
    error,
    model::*,
    Result,
};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};
#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
    pub cache: Cache,
    pub fetched: Cache,
    pub progress: crate::backend::Progress,
    pub runtime: std::sync::Arc<crate::metadata::Runtime>,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        if root.join("state.sqlite3").exists() {
            let probe = rusqlite::Connection::open_with_flags(
                root.join("state.sqlite3"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let version: i64 = probe.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version != 0 && version != 3 {
                return Err(error(format!("unsupported store schema {version}; use a fresh --state-dir (this binary requires schema 3)")));
            }
        }
        std::fs::create_dir_all(root)?;
        let root = std::fs::canonicalize(root)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        let s = Self {
            cache: Cache::new(root.join("content"))?,
            fetched: Cache::new(root.join("fetched"))?,
            root,
            progress: std::sync::Arc::new(|_| {}),
            runtime: Default::default(),
        };
        let _l = Lock::take(&s.root.join("schema.lock"))?;
        let c = s.conn()?;
        let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 3 {
            return Err(error("database is newer than this binary"));
        }
        if version == 3 {
            let (epoch, stamp) =
                c.query_row("SELECT epoch,stamp FROM metadata_epoch", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
            s.runtime.observe(epoch, stamp);
            drop(c);
            s.runtime
                .limit(s.setting("metadata_cache_bytes", 512 * 1024 * 1024)? as usize);
            return Ok(s);
        }
        c.execute_batch("BEGIN IMMEDIATE;
 CREATE TABLE IF NOT EXISTS repo_cache(storage TEXT PRIMARY KEY,origin TEXT NOT NULL UNIQUE);
 CREATE TABLE IF NOT EXISTS repos(id TEXT PRIMARY KEY,origin TEXT NOT NULL,storage TEXT NOT NULL REFERENCES repo_cache(storage));
 CREATE TABLE IF NOT EXISTS revisions(id INTEGER PRIMARY KEY,parent INTEGER REFERENCES revisions(id),map TEXT NOT NULL,delta TEXT NOT NULL,created INTEGER NOT NULL DEFAULT(unixepoch()));
 CREATE TABLE IF NOT EXISTS workspaces(name TEXT PRIMARY KEY,head INTEGER NOT NULL REFERENCES revisions(id),draft TEXT NOT NULL,dirty INTEGER NOT NULL DEFAULT 0,changed INTEGER NOT NULL DEFAULT(unixepoch()),last_snapshot INTEGER NOT NULL DEFAULT(unixepoch()),activation TEXT);
 CREATE TABLE IF NOT EXISTS snapshots(workspace TEXT NOT NULL REFERENCES workspaces(name),number INTEGER NOT NULL,revision INTEGER NOT NULL REFERENCES revisions(id),draft TEXT NOT NULL,pinned INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(workspace,number));
 CREATE TABLE IF NOT EXISTS activations(id TEXT PRIMARY KEY,workspace TEXT NOT NULL,source INTEGER NOT NULL,proposal TEXT NOT NULL,candidate TEXT NOT NULL,conflicts TEXT NOT NULL,state TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS exports(id INTEGER PRIMARY KEY,workspace TEXT NOT NULL,revision INTEGER NOT NULL,path TEXT NOT NULL,manifest TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS objects(repo TEXT NOT NULL,oid TEXT NOT NULL,content TEXT NOT NULL,size INTEGER NOT NULL,accessed INTEGER NOT NULL DEFAULT(unixepoch()),PRIMARY KEY(repo,oid));
 CREATE TABLE IF NOT EXISTS reviews(export_path TEXT NOT NULL,mount TEXT NOT NULL,branch TEXT NOT NULL,head TEXT NOT NULL,url TEXT,state TEXT NOT NULL,PRIMARY KEY(export_path,mount));
 CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS tree_cache(repo TEXT NOT NULL,tree_key TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(repo,tree_key));
 CREATE TABLE IF NOT EXISTS attributes(path TEXT PRIMARY KEY,fingerprint TEXT NOT NULL,stamp INTEGER NOT NULL);
")?;
        c.execute_batch("
CREATE TABLE IF NOT EXISTS metadata_epoch(epoch INTEGER NOT NULL,stamp INTEGER NOT NULL);
INSERT INTO metadata_epoch SELECT 0,0 WHERE NOT EXISTS(SELECT 1 FROM metadata_epoch);
CREATE TABLE IF NOT EXISTS revision_stamps(revision INTEGER PRIMARY KEY,stamp INTEGER NOT NULL);
INSERT OR IGNORE INTO revision_stamps SELECT id,created*1000000000 FROM revisions;
CREATE TABLE IF NOT EXISTS metadata_checkpoints(revision INTEGER PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS metadata_trees(repo TEXT NOT NULL,tree_key TEXT NOT NULL,PRIMARY KEY(repo,tree_key));
CREATE TABLE IF NOT EXISTS metadata_entries(repo TEXT NOT NULL,tree_key TEXT NOT NULL,parent TEXT NOT NULL,name TEXT NOT NULL,entry TEXT NOT NULL,PRIMARY KEY(repo,tree_key,parent,name)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS metadata_sizes(content TEXT PRIMARY KEY,size INTEGER NOT NULL);
INSERT OR IGNORE INTO metadata_sizes SELECT 'git:'||repo||':'||oid,size FROM objects;
")?;
        for table in ["workspaces", "revisions", "snapshots", "activations"] {
            for op in ["INSERT", "UPDATE", "DELETE"] {
                c.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS metadata_{table}_{op} AFTER {op} ON {table} BEGIN UPDATE metadata_epoch SET epoch=epoch+1,stamp=max(stamp+1,CAST(unixepoch('subsec')*1000000000 AS INTEGER)); END;"))?;
            }
        }
        c.execute_batch("CREATE TRIGGER IF NOT EXISTS metadata_revision_stamp AFTER INSERT ON revisions BEGIN INSERT OR REPLACE INTO revision_stamps VALUES(NEW.id,max((SELECT stamp+1 FROM metadata_epoch),CAST(unixepoch('subsec')*1000000000 AS INTEGER))); END;")?;
        c.execute_batch("PRAGMA user_version=3;COMMIT;")?;
        let (epoch, stamp) = c.query_row("SELECT epoch,stamp FROM metadata_epoch", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        s.runtime.observe(epoch, stamp);
        drop(c);
        Ok(s)
    }
    pub fn conn(&self) -> Result<crate::metadata::Connection> {
        crate::metadata::Connection::open(&self.root, self.runtime.clone())
    }
    pub fn workspace_lock(&self, name: &str) -> Result<Lock> {
        if !name_ok(name) {
            return Err(error("invalid workspace name"));
        }
        Lock::take(&self.root.join("locks").join(format!("workspace-{name}")))
    }
    pub fn create(&self, name: &str, draft: &str) -> Result<()> {
        let _l = self.workspace_lock(name)?;
        let mut c = self.conn()?;
        let t = c.transaction()?;
        t.execute(
            "INSERT INTO revisions(map,delta) VALUES(?1,'[]')",
            [serde_json::to_string(&Map::default())?],
        )?;
        t.execute(
            "INSERT INTO workspaces(name,head,draft) VALUES(?1,?2,?3)",
            params![name, t.last_insert_rowid(), draft],
        )?;
        t.commit()?;
        self.observe_committed()?;
        Ok(())
    }
    pub fn names(&self) -> Result<Vec<String>> {
        Ok(self
            .cached_value("names", || {
                let c = self.conn()?;
                let mut q = c.prepare_cached("SELECT name FROM workspaces ORDER BY name")?;
                let rows = q
                    .query_map([], |r| r.get(0))?
                    .collect::<std::result::Result<Vec<String>, _>>()?;
                Ok(rows)
            })?
            .as_ref()
            .clone())
    }
    pub fn head(&self, name: &str) -> Result<i64> {
        Ok(*self.cached_value(&format!("head:{name}"), || {
            Ok(self.conn()?.query_row(
                "SELECT head FROM workspaces WHERE name=?1",
                [name],
                |r| r.get::<_, i64>(0),
            )?)
        })?)
    }
    pub fn draft(&self, name: &str) -> Result<String> {
        Ok(self
            .cached_value(&format!("draft:{name}"), || {
                Ok(self.conn()?.query_row(
                    "SELECT draft FROM workspaces WHERE name=?1",
                    [name],
                    |r| r.get::<_, String>(0),
                )?)
            })?
            .as_ref()
            .clone())
    }
    pub fn set_draft(&self, name: &str, data: &str) -> Result<()> {
        let count = self.conn()?.execute(
            "UPDATE workspaces SET draft=?2,dirty=1,changed=unixepoch() WHERE name=?1",
            params![name, data],
        )?;
        if count != 1 {
            return Err(error("unknown workspace"));
        }
        Ok(())
    }
    pub fn state(&self, id: i64) -> Result<(Map, Overlay)> {
        let state = self.revision_snapshot(id)?;
        Ok((
            state.map.clone(),
            state
                .overlay
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ))
    }
    pub fn frozen(&self, name: &str) -> Result<bool> {
        Ok(*self.cached_value(&format!("frozen:{name}"), || {
            Ok(self.conn()?.query_row(
                "SELECT activation IS NOT NULL FROM workspaces WHERE name=?1",
                [name],
                |r| r.get::<_, bool>(0),
            )?)
        })?)
    }
    pub fn apply(&self, name: &str, deltas: Vec<Delta>) -> Result<i64> {
        let _l = self.workspace_lock(name)?;
        if self.frozen(name)? {
            return Err(error("workspace activation in progress: writes frozen"));
        }
        let head = self.head(name)?;
        let (map, _) = self.state(head)?;
        self.publish(name, Some(head), &map, &deltas)
    }
    pub fn publish(
        &self,
        name: &str,
        parent: Option<i64>,
        map: &Map,
        deltas: &[Delta],
    ) -> Result<i64> {
        let mut c = self.conn()?;
        let t = c.transaction()?;
        t.execute(
            "INSERT INTO revisions(parent,map,delta) VALUES(?1,?2,?3)",
            params![
                parent,
                serde_json::to_string(map)?,
                serde_json::to_string(deltas)?
            ],
        )?;
        let id = t.last_insert_rowid();
        t.execute(
            "UPDATE workspaces SET head=?2,dirty=1,changed=unixepoch() WHERE name=?1",
            params![name, id],
        )?;
        crate::failpoint("revision-before-commit");
        t.commit()?;
        {
            let mut changed = self.runtime.changed.lock().unwrap();
            if parent.is_none() {
                changed.insert(format!("workspaces/{name}/"));
            } else {
                for d in deltas {
                    if let Some(m) = map.mount.iter().find(|m| m.id == d.mount) {
                        let p = join(&m.path, &d.path);
                        changed.insert(format!("workspaces/{name}/depot/{p}"));
                        changed.insert(
                            format!("workspaces/{name}/depot/{}", crate::metadata::split(&p).0)
                                .trim_end_matches('/')
                                .to_string(),
                        );
                    }
                }
            }
        }
        self.observe_committed()?;
        Ok(id)
    }
    pub fn snapshot(&self, name: &str) -> Result<i64> {
        let _l = self.workspace_lock(name)?;
        self.snapshot_unlocked(name)
    }
    pub fn snapshot_unlocked(&self, name: &str) -> Result<i64> {
        let mut c = self.conn()?;
        let t = c.transaction()?;
        let n: i64 = t.query_row(
            "SELECT COALESCE(MAX(number),0)+1 FROM snapshots WHERE workspace=?1",
            [name],
            |r| r.get(0),
        )?;
        t.execute("INSERT INTO snapshots(workspace,number,revision,draft) SELECT name,?2,head,draft FROM workspaces WHERE name=?1",params![name,n])?;
        t.execute(
            "UPDATE workspaces SET dirty=0,last_snapshot=unixepoch() WHERE name=?1",
            [name],
        )?;
        crate::failpoint("snapshot-before-commit");
        t.commit()?;
        self.observe_committed()?;
        Ok(n)
    }
    pub fn snapshot_revision(&self, name: &str, n: i64) -> Result<(i64, String)> {
        let key = format!("snapshot:{name}:{n}");
        if let Some(v) = self.runtime.get::<(i64, String)>(&key) {
            return Ok(v.as_ref().clone());
        }
        let value: (i64, String) = self.conn()?.query_row(
            "SELECT revision,draft FROM snapshots WHERE workspace=?1 AND number=?2",
            params![name, n],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let bytes = value.1.len() + 128;
        Ok(self.runtime.put(key, value, bytes)?.as_ref().clone())
    }
    pub fn snapshots(&self, name: &str) -> Result<Vec<i64>> {
        let c = self.conn()?;
        let mut q = c.prepare("SELECT number FROM snapshots WHERE workspace=?1 ORDER BY number")?;
        let v = q
            .query_map([name], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }
    pub fn auto_snapshots(&self) -> Result<()> {
        let c = self.conn()?;
        let mut q=c.prepare("SELECT name FROM workspaces WHERE dirty=1 AND (unixepoch()-changed>=?1 OR unixepoch()-last_snapshot>=?2)")?;
        let names = q
            .query_map(
                params![
                    self.setting("snapshot_idle_seconds", 2)?,
                    self.setting("snapshot_max_seconds", 30)?
                ],
                |r| r.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for n in names {
            self.snapshot(&n)?;
        }
        Ok(())
    }
    pub fn cached(&self, repo: &str, oid: &str) -> Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row(
                "SELECT content FROM objects WHERE repo=?1 AND oid=?2",
                params![repo, oid],
                |r| r.get(0),
            )
            .optional()?)
    }
}
impl Store {
    pub fn attribute_time(&self, path: &str, fingerprint: &str) -> Result<std::time::SystemTime> {
        let c = self.conn()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos() as i64;
        c.execute("INSERT INTO attributes(path,fingerprint,stamp) VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET fingerprint=excluded.fingerprint,stamp=excluded.stamp WHERE attributes.fingerprint!=excluded.fingerprint",params![path,fingerprint,now])?;
        let stamp: i64 =
            c.query_row("SELECT stamp FROM attributes WHERE path=?1", [path], |r| {
                r.get(0)
            })?;
        Ok(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(stamp as u64))
    }
}
impl Store {
    pub fn setting(&self, key: &str, default: i64) -> Result<i64> {
        Ok(self
            .conn()?
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?
            .unwrap_or(default))
    }
}
