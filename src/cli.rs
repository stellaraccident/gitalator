use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Parser)]
#[command(version, about = "Depot-backed federated workspaces")]
pub struct Cli {
    #[arg(long,global=true,default_value_os_t=crate::store::default_root())]
    pub state_dir: PathBuf,
    #[arg(long, global = true)]
    pub offline: bool,
    #[command(subcommand)]
    pub command: Cmd,
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum Cmd {
    /// Show active operations without waiting for workspace locks.
    Activity,
    /// Cancel an operation reported by activity.
    Cancel {
        id: u64,
    },
    Daemon {
        #[command(subcommand)]
        command: DaemonCmd,
    },
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCmd,
    },
    Config {
        #[arg(long)]
        mount: PathBuf,
    },
    Status {
        workspace: String,
    },
    Diff {
        workspace: String,
    },
    Dir {
        workspace: String,
    },
    Revert {
        workspace: String,
        path: String,
    },
    Validate {
        workspace: String,
    },
    Checkpoint {
        workspace: String,
        #[arg(short, long)]
        message: String,
    },
    Depot {
        #[command(subcommand)]
        command: DepotCmd,
    },
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum DaemonCmd {
    Start {
        #[arg(long)]
        detached: bool,
    },
    Run,
    Status,
    Stop {
        #[arg(long)]
        lazy: bool,
    },
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum WorkspaceCmd {
    Create {
        name: String,
        #[arg(long)]
        from: String,
        #[arg(long, default_value = "HEAD")]
        revision: String,
    },
    List,
    Fork {
        name: String,
        #[arg(long)]
        from: String,
    },
    Clone {
        name: String,
        #[arg(long)]
        from: String,
    },
    Remove {
        name: String,
    },
}
#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum DepotCmd {
    Begin {
        workspace: String,
    },
    End {
        workspace: String,
        token: String,
    },
    Edit {
        workspace: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
}

impl Cmd {
    pub fn label(&self) -> String {
        match self {
            Self::Workspace { command } => match command {
                WorkspaceCmd::Create { name, .. } => format!("workspace create {name}"),
                WorkspaceCmd::Fork { name, .. } => format!("workspace fork {name}"),
                WorkspaceCmd::Clone { name, .. } => format!("workspace clone {name}"),
                WorkspaceCmd::Remove { name } => format!("workspace remove {name}"),
                WorkspaceCmd::List => "workspace list".into(),
            },
            Self::Status { workspace } => format!("status {workspace}"),
            Self::Diff { workspace } => format!("diff {workspace}"),
            Self::Dir { workspace } => format!("dir {workspace}"),
            Self::Revert { workspace, path } => format!("revert {workspace}: {path}"),
            Self::Validate { workspace } => format!("validate {workspace}"),
            Self::Checkpoint { workspace, .. } => format!("checkpoint {workspace}"),
            Self::Depot { .. } => "depot edit session".into(),
            Self::Daemon { .. } => "daemon lifecycle".into(),
            Self::Config { .. } => "config".into(),
            Self::Activity => "activity".into(),
            Self::Cancel { id } => format!("cancel {id}"),
        }
    }
}
