use clap::Parser;
use gitalator::{
    cli::{Cli, Cmd},
    db::Store,
    Result,
};
fn execute() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Unmount { path } = cli.command {
        let status = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg(path)
            .status()?;
        if !status.success() {
            return Err("unmount failed".into());
        }
        return Ok(());
    }
    let store = Store::open(&cli.state_dir)?;
    if let Cmd::Mount { path } = cli.command {
        let _lock = gitalator::cache::Lock::take(&store.root.join("daemon.lock"))?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = gitalator::cli::server(store.clone(), stop.clone())?;
        let result = gitalator::vfs::mount(store, &path);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = server.join();
        return result;
    }
    let value = if let Some(v) = gitalator::cli::request(&store.root, &cli.command)? {
        v
    } else {
        gitalator::cli::dispatch(&store, cli.command)?
    };
    if cli.json {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}
fn main() {
    if let Err(e) = execute() {
        eprintln!("gtl: {e}");
        std::process::exit(1)
    }
}
