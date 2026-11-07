use crate::{error, Result};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
static SEQ: AtomicU64 = AtomicU64::new(1);
pub fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
pub struct Lock(File);
impl Lock {
    pub fn try_take(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(error("another daemon already owns this state directory"));
        }
        Ok(Self(f))
    }
    pub fn take(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let start = Instant::now();
        let timeout = std::env::var("GTL_LOCK_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        loop {
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::WouldBlock {
                return Err(err.into());
            }
            if start.elapsed() >= std::time::Duration::from_secs(timeout) {
                return Err(error("resource lock timed out"));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(Self(f))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| error("missing parent"))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".pending-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(parent)?.sync_all()?;
    crate::failpoint("payload-after-publish");
    Ok(())
}
#[derive(Clone)]
pub struct Cache {
    pub root: PathBuf,
}
impl Cache {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("objects"))?;
        Ok(Self { root })
    }
    pub fn put(&self, b: &[u8]) -> Result<String> {
        let id = hash(b);
        let _guard = Lock::take(&self.root.join("locks").join(&id))?;
        let p = self.root.join("objects").join(&id);
        if !p.exists() {
            atomic(&p, b)?;
        } else if hash(&fs::read(&p)?) != id {
            return Err(error("cache integrity failure"));
        }
        Ok(id)
    }
    pub fn get(&self, id: &str) -> Result<Vec<u8>> {
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(error("invalid content key"));
        }
        let b = fs::read(self.root.join("objects").join(id))?;
        if hash(&b) != id {
            return Err(error("cache integrity failure"));
        }
        Ok(b)
    }
}
pub struct Span {
    root: PathBuf,
    id: u64,
    op: String,
    start: Instant,
}
impl Span {
    pub fn new(root: &Path, op: &str) -> Self {
        let s = Self {
            root: root.into(),
            id: SEQ.fetch_add(1, Ordering::Relaxed),
            op: op.into(),
            start: Instant::now(),
        };
        s.event("start", None);
        s
    }
    pub fn event(&self, status: &str, detail: Option<&str>) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let v = serde_json::json!({"timestamp_ms":timestamp_ms,"pid":std::process::id(),"operation_id":self.id,"operation":self.op,"status":status,"elapsed_ms":self.start.elapsed().as_millis(),"detail":detail});
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("operations.jsonl"))
            .and_then(|mut f| f.write_all(format!("{v}\n").as_bytes()));
        if let Err(e) = result {
            eprintln!("gtl: operation log write failed: {e}");
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        self.event("finished", None)
    }
}
