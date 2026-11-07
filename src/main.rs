use clap::Parser;
use gitalator::{
    cli::{Cli, Cmd, DaemonCmd, SubmoduleCmd},
    db::Store,
    Result,
};
fn execute() -> Result<()> {
    let mut cli = Cli::parse();
    if cli.state_dir.is_relative() {
        cli.state_dir = std::env::current_dir()?.join(&cli.state_dir);
    }
    cli.command.absolutize()?;
    match &cli.command {
        Cmd::Daemon {
            command: DaemonCmd::Start { detached },
        } => {
            let info = gitalator::daemon::start(&cli.state_dir, *detached)?;
            return if cli.json {
                print_result(info, true, false)
            } else {
                Ok(())
            };
        }
        Cmd::Daemon {
            command: DaemonCmd::Run { mode },
        } => return gitalator::daemon::run(&cli.state_dir, None, mode, true),
        Cmd::Mount { path } => {
            return gitalator::daemon::run(&cli.state_dir, Some(path), "foreground", false)
        }
        Cmd::Unmount { path } => {
            let status = std::process::Command::new("fusermount3")
                .arg("-u")
                .arg(path)
                .status()?;
            return if status.success() {
                Ok(())
            } else {
                Err("unmount failed".into())
            };
        }
        _ => {}
    }
    if let Cmd::Submodules {
        command: SubmoduleCmd::Expand { workspace, .. },
    } = &mut cli.command
    {
        if workspace.is_none() {
            let mount = if cli.offline {
                gitalator::config::Config::load(&cli.state_dir)?.mount_path(&cli.state_dir)
            } else {
                let info = gitalator::cli::request(
                    &cli.state_dir,
                    &Cmd::Daemon {
                        command: DaemonCmd::Status,
                    },
                )?
                .ok_or("daemon is not running; run gtl daemon start")?;
                std::path::PathBuf::from(info["mount"].as_str().ok_or("daemon omitted mount path")?)
            };
            *workspace = Some(gitalator::namespace::context(&mount, &std::env::current_dir()?)?.0);
        }
    }
    let path_only = matches!(cli.command, Cmd::NewWorkspace { .. } | Cmd::Dir { .. });
    if !cli.offline {
        if let Some(value) = gitalator::cli::request(&cli.state_dir, &cli.command)? {
            return print_result(value, cli.json, path_only);
        }
        if matches!(
            cli.command,
            Cmd::Daemon {
                command: DaemonCmd::Status | DaemonCmd::Stop { .. }
            }
        ) {
            return print_result(serde_json::json!({"running":false}), cli.json, false);
        }
        return Err("daemon is not running; run `gtl daemon start` (or use --offline for explicit offline administration)".into());
    }
    if matches!(cli.command, Cmd::Daemon { .. }) {
        return Err("daemon commands cannot run --offline".into());
    }
    Store::check_schema(&cli.state_dir)?;
    std::fs::create_dir_all(&cli.state_dir)?;
    let _ownership = gitalator::cache::Lock::try_take(&cli.state_dir.join("daemon.lock"))?;
    let mut store = Store::open(&cli.state_dir)?;
    gitalator::config::Config::load(&store.root)?.apply(&store)?;
    store.progress = std::sync::Arc::new(|line| eprintln!("gtl: {line}"));
    print_result(
        gitalator::cli::dispatch(&store, cli.command)?,
        cli.json,
        path_only,
    )
}
fn print_result(value: serde_json::Value, json: bool, path_only: bool) -> Result<()> {
    if !json && path_only {
        println!("{}", value["path"].as_str().ok_or("missing result path")?);
    } else if json {
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
        gitalator::logging::message(&format!("gtl: {e}"));
        std::process::exit(1)
    }
}
