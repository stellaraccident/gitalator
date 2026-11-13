use clap::Parser;
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
    let mut cli = Cli::parse();
    if let Cmd::Agents { ref chapter } = cli.command {
        print!("{}", gitalator::agents::chapter(chapter.as_ref()));
        return Ok(());
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
