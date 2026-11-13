use clap::{CommandFactory, FromArgMatches};
use gitalator::{
    cli::{Cli, Cmd, DaemonCmd, DepotCmd},
    daemon,
    store::{absolute, Store},
    util::Lock,
    vfs::Core,
    workspace::dispatch,
    Result,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
fn invoke(root: &Path, offline: bool, cmd: Cmd) -> Result<serde_json::Value> {
    if offline {
        gitalator::activity::init(root)?;
        let task = gitalator::activity::Task::new(cmd.label());
        let _context = task.enter();
        std::fs::create_dir_all(root)?;
        let _lock = Lock::try_take(&root.join("daemon.lock"))?;
        gitalator::activity::event("info", &format!("Begin {}", cmd.label()));
        let result = (|| {
            let store = Store::open(root)?;
            dispatch(&Core::new(Arc::new(Mutex::new(store))), cmd)
        })();
        match &result {
            Ok(_) => gitalator::activity::event("info", "Completed"),
            Err(e) if gitalator::activity::interrupted(&**e) => {
                gitalator::activity::event("info", "Cancelled")
            }
            Err(e) => gitalator::activity::event("error", &format!("Failed: {e}")),
        }
        result
    } else {
        daemon::request(root, &cmd)
    }
}
fn run() -> Result<()> {
    let matches = Cli::command().get_matches();
    let explicit_state =
        matches.value_source("state_dir") == Some(clap::parser::ValueSource::CommandLine);
    let mut cli = Cli::from_arg_matches(&matches)?;
    if let Cmd::Agents { ref chapter } = cli.command {
        print!("{}", gitalator::agents::chapter(chapter.as_ref()));
        return Ok(());
    }
    if let Cmd::Mount { command } = &mut cli.command {
        let (at, workspace, workspace_id) = command.location_mut();
        if workspace.is_none() {
            let found = gitalator::mounts::discover(&std::env::current_dir()?)?;
            if explicit_state
                && std::fs::canonicalize(absolute(&cli.state_dir)?)
                    .unwrap_or(absolute(&cli.state_dir)?)
                    != found.state
            {
                return Err("current workspace belongs to a different --state-dir".into());
            }
            cli.state_dir = found.state;
            *workspace = Some(found.name);
            *workspace_id = Some(found.id);
        }
        if at == "." {
            *at = String::new();
        }
        if let gitalator::cli::MountCmd::Add { url, .. } = command {
            let path = Path::new(url);
            if path.exists() {
                *url = std::fs::canonicalize(path)?
                    .to_str()
                    .ok_or("non-UTF8 repository path")?
                    .into();
            }
        }
    }

    let root = absolute(&cli.state_dir)?;
    if cli.offline {
        extern "C" fn interrupt(_: i32) {
            gitalator::activity::SHUTDOWN.store(true, std::sync::atomic::Ordering::Release);
        }
        unsafe {
            libc::signal(libc::SIGINT, interrupt as *const () as usize);
            libc::signal(libc::SIGTERM, interrupt as *const () as usize);
        }
    }
    if let Cmd::Workspace {
        command: gitalator::cli::WorkspaceCmd::Create { from, .. },
    } = &mut cli.command
    {
        let path = Path::new(from);
        if path.exists() {
            *from = std::fs::canonicalize(path)?
                .to_str()
                .ok_or("non-UTF8 origin")?
                .into();
        }
    }
    let result = match cli.command {
        Cmd::Daemon {
            command: DaemonCmd::Run,
        } => {
            daemon::run(&root)?;
            return Ok(());
        }
        Cmd::Daemon {
            command: DaemonCmd::Start { detached },
        } => daemon::start(&root, detached)?,
        Cmd::Daemon {
            command: DaemonCmd::Status,
        } => daemon::request(
            &root,
            &Cmd::Daemon {
                command: DaemonCmd::Status,
            },
        )
        .unwrap_or(serde_json::json!({"running":false})),
        Cmd::Depot {
            command: DepotCmd::Edit { workspace, command },
        } => {
            let info = invoke(
                &root,
                cli.offline,
                Cmd::Depot {
                    command: DepotCmd::Begin {
                        workspace: workspace.clone(),
                    },
                },
            )?;
            let token = info["token"].as_str().unwrap().to_string();
            let status = std::process::Command::new(&command[0])
                .args(&command[1..])
                .current_dir(info["worktree"].as_str().unwrap())
                .status();
            eprintln!("gtl: edit session token {token}");
            if !status?.success() {
                return Err("command failed; workspace remains suspended; repair and use depot end with the token".into());
            }
            invoke(
                &root,
                cli.offline,
                Cmd::Depot {
                    command: DepotCmd::End { workspace, token },
                },
            )?
        }
        command => invoke(&root, cli.offline, command)?,
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("gtl: {e}");
        std::process::exit(1);
    }
}
