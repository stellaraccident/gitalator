use clap::{Parser, Subcommand, ValueEnum};
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
    /// Print the agent guide or a task chapter (no server required).
    Agents {
        #[command(subcommand)]
        chapter: Option<AgentChapter>,
    },
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
    Mount {
        #[command(subcommand)]
        command: MountCmd,
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
    /// Initialize an empty workspace with a new local depot (no remote).
    Init {
        name: String,
    },
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
            Self::Mount { command } => match command {
                MountCmd::Add { .. } => "mount add",
                MountCmd::Remove { .. } => "mount remove",
                MountCmd::Submodules { .. } => "mount submodules",
            }
            .into(),
            Self::Workspace { command } => match command {
                WorkspaceCmd::Init { name } => format!("workspace init {name}"),
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
            Self::Agents { .. } => "agent guide".into(),
            Self::Activity => "activity".into(),
            Self::Cancel { id } => format!("cancel {id}"),
        }
    }
}

#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum AgentChapter {
    /// Add pinned repository mounts and choose submodule import depth.
    MountAdd,
    /// Remove a mount, optionally including its descendant mounts.
    MountRemove,
    /// Expand submodules at an existing mount's current pin.
    MountSubmodules,
    /// Create an empty workspace or attach an existing depot.
    WorkspaceCreate,
}

#[derive(Subcommand, Serialize, Deserialize, Debug)]
pub enum MountCmd {
    /// Add a repository at a workspace-relative path; "." means the root.
    Add {
        url: String,
        #[arg(long)]
        at: String,
        #[arg(long, default_value = "HEAD")]
        revision: String,
        #[arg(long, value_enum, default_value = "none")]
        submodules: Submodules,
        /// Skip child repositories whose fetch fails and report each error.
        #[arg(long)]
        #[serde(default)]
        skip_unavailable: bool,
        /// Defaults to the workspace containing the current directory.
        #[arg(long)]
        workspace: Option<String>,
        #[arg(skip)]
        workspace_id: Option<String>,
    },
    /// Remove a mount while preserving depot overlay files.
    Remove {
        #[arg(long)]
        at: String,
        /// Also remove every mount beneath this path.
        #[arg(long)]
        recursive: bool,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(skip)]
        workspace_id: Option<String>,
    },
    /// Import missing submodules from the mount's current pinned commit.
    Submodules {
        #[arg(long)]
        at: String,
        #[arg(long, value_enum, default_value = "first-level")]
        depth: SubmoduleDepth,
        /// Keep successful imports when a child repository cannot be fetched.
        #[arg(long)]
        #[serde(default)]
        skip_unavailable: bool,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(skip)]
        workspace_id: Option<String>,
    },
}
#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Submodules {
    None,
    FirstLevel,
    Recursive,
}

#[derive(Clone, Copy, Debug, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubmoduleDepth {
    FirstLevel,
    Recursive,
}
impl From<SubmoduleDepth> for Submodules {
    fn from(depth: SubmoduleDepth) -> Self {
        match depth {
            SubmoduleDepth::FirstLevel => Self::FirstLevel,
            SubmoduleDepth::Recursive => Self::Recursive,
        }
    }
}
impl MountCmd {
    pub fn location_mut(&mut self) -> (&mut String, &mut Option<String>, &mut Option<String>) {
        match self {
            Self::Add {
                at,
                workspace,
                workspace_id,
                ..
            }
            | Self::Remove {
                at,
                workspace,
                workspace_id,
                ..
            }
            | Self::Submodules {
                at,
                workspace,
                workspace_id,
                ..
            } => (at, workspace, workspace_id),
        }
    }
}
