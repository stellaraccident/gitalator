//! Task guides embedded in the executable; reading them has no state side effects.
use crate::cli::AgentChapter;
pub fn chapter(chapter: Option<&AgentChapter>) -> &'static str {
    match chapter {
        None => include_str!("../docs/agents/index.md"),
        Some(AgentChapter::MountRemove) => include_str!("../docs/agents/mount-remove.md"),
        Some(AgentChapter::MountSubmodules) => include_str!("../docs/agents/mount-submodules.md"),
        Some(AgentChapter::MountAdd) => include_str!("../docs/agents/mount-add.md"),
        Some(AgentChapter::WorkspaceCreate) => include_str!("../docs/agents/workspace-create.md"),
    }
}
