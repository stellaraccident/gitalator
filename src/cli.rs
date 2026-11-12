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
