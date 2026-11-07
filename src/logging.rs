use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
static ROOT: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
pub fn initialize(root: &Path) -> crate::Result<()> {
    std::fs::create_dir_all(root.join("logs"))?;
    *ROOT.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(root.to_path_buf());
    Ok(())
}
pub fn message(text: &str) {
    let clean = crate::backend::redact(text);
    eprintln!("{clean}");
    if let Some(root) = ROOT
        .get()
        .and_then(|r| r.lock().ok().and_then(|r| r.clone()))
    {
        if let Err(e) = crate::cache::append_named_log(
            &root,
            "daemon.log",
            &format!(
                "{} {clean}\n",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            ),
        ) {
            eprintln!("gtl: daemon log write failed: {e}");
        }
    }
}
