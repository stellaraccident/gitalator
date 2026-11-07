use clap::Parser;
use gitalator::{
    cli::{Cli, Cmd},
    db::Store,
    Result,
};
fn execute() -> Result<()> {
    let mut cli = Cli::parse();
    cli.command.absolutize()?;
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
    // A mounted client must not open/migrate a database owned by another binary.
    if !matches!(cli.command, Cmd::Mount { .. }) {
        if let Some(value) = gitalator::cli::request(&cli.state_dir, &cli.command)? {
            return print_result(value, cli.json);
        }
    }
    std::fs::create_dir_all(&cli.state_dir)?;
    let _ownership = gitalator::cache::Lock::try_take(&cli.state_dir.join("daemon.lock"))?;
    // Recheck after taking the ownership lock to close the mount/start race.
    if matches!(cli.command, Cmd::Mount { .. }) && cli.state_dir.join("control.sock").exists() {
        std::fs::remove_file(cli.state_dir.join("control.sock"))?;
    }
    if cli.state_dir.join("control.sock").exists() {
        return Err(
            "stale control socket: restart the owning daemon before using this state".into(),
        );
    }
    let mut store = Store::open(&cli.state_dir)?;
    store.progress = std::sync::Arc::new(|line| eprintln!("gtl: {line}"));
    if let Cmd::Mount { path } = cli.command {
        store
            .runtime
            .activity
            .enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "gtl: mounting {}; detailed log: {} (8 MiB × 4 files)",
            path.display(),
            store.root.join("operations.jsonl").display()
        );
        let lifecycle = gitalator::cache::Span::new(&store.root, "mount");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = gitalator::cli::server(store.clone(), stop.clone())?;
        let activity = store.runtime.activity.clone();
        let report_stop = stop.clone();
        let reporter = std::thread::spawn(move || {
            while !report_stop.load(std::sync::atomic::Ordering::Relaxed) {
                activity.report();
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        });
        let result = gitalator::vfs::mount(store, &path);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = server.join();
        let _ = reporter.join();
        lifecycle.event(
            if result.is_ok() { "stopped" } else { "failed" },
            result.as_ref().err().map(|e| e.to_string()).as_deref(),
        );
        if result.is_ok() {
            eprintln!("gtl: mount stopped");
        }
        return result;
    }
    let value = gitalator::cli::dispatch(&store, cli.command)?;
    print_result(value, cli.json)
}
fn print_result(value: serde_json::Value, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    if value
        .get("failures")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|v| !v.is_empty())
    {
        return Err("warmup completed with failures".into());
    }
    Ok(())
}
fn main() {
    if let Err(e) = execute() {
        eprintln!("gtl: {e}");
        std::process::exit(1)
    }
}
