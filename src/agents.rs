//! Task guides embedded in the executable; reading them has no state side effects.
use crate::cli::AgentChapter;
pub fn chapter(chapter: Option<&AgentChapter>) -> &'static str {
    match chapter {
        None => include_str!("../docs/agents/index.md"),
        Some(AgentChapter::WorkspaceCreate) => include_str!("../docs/agents/workspace-create.md"),
    }
}
