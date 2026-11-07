//! The only mapping between public mount paths and logical views/control files.
use crate::{db::Store, error, ops::View, Result};
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Dir,
    Control(String, Control),
    Source(View, String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Control {
    Draft,
    Active,
    Status,
    SnapshotDraft(i64),
    SnapshotActive(i64),
    TxStatus,
    TxMap,
    TxConflicts,
}
pub(crate) fn node(store: &Store, path: &str) -> Result<Node> {
    let parts: Vec<_> = path.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        []
        | ["workspaces"]
        | ["readonly"]
        | ["snapshots"]
        | ["control"]
        | ["control", "workspaces"]
        | ["control", "snapshots"]
        | ["control", "transactions"] => Ok(Node::Dir),
        ["workspaces", n, rest @ ..] => Ok(Node::Source(View::Live((*n).into()), rest.join("/"))),
        ["readonly", n, rest @ ..] => Ok(Node::Source(View::Base((*n).into()), rest.join("/"))),
        ["snapshots", n] | ["control", "workspaces", n] | ["control", "snapshots", n] => {
            store.head(n)?;
            Ok(Node::Dir)
        }
        ["snapshots", n, num, rest @ ..] => Ok(Node::Source(
            View::Snapshot((*n).into(), num.parse()?, false),
            rest.join("/"),
        )),
        ["control", "workspaces", n, "VERSION_MAP"] => {
            Ok(Node::Control((*n).into(), Control::Draft))
        }
        ["control", "workspaces", n, "ACTIVE_VERSION_MAP"] => {
            Ok(Node::Control((*n).into(), Control::Active))
        }
        ["control", "workspaces", n, "STATUS"] => Ok(Node::Control((*n).into(), Control::Status)),
        ["control", "snapshots", n, num] => {
            store.snapshot_revision(n, num.parse()?)?;
            Ok(Node::Dir)
        }
        ["control", "snapshots", n, num, "VERSION_MAP"] => Ok(Node::Control(
            (*n).into(),
            Control::SnapshotDraft(num.parse()?),
        )),
        ["control", "snapshots", n, num, "ACTIVE_VERSION_MAP"] => Ok(Node::Control(
            (*n).into(),
            Control::SnapshotActive(num.parse()?),
        )),
        ["control", "snapshots", n, num, "readonly", rest @ ..] => Ok(Node::Source(
            View::Snapshot((*n).into(), num.parse()?, true),
            rest.join("/"),
        )),
        ["control", "transactions", id] => {
            store.conflict_list(id)?;
            Ok(Node::Dir)
        }
        ["control", "transactions", id, "source", rest @ ..] => Ok(Node::Source(
            View::Transaction((*id).into()),
            rest.join("/"),
        )),
        ["control", "transactions", id, "STATUS"] => {
            Ok(Node::Control((*id).into(), Control::TxStatus))
        }
        ["control", "transactions", id, "PROPOSED_VERSION_MAP"] => {
            Ok(Node::Control((*id).into(), Control::TxMap))
        }
        ["control", "transactions", id, "conflicts.json"] => {
            Ok(Node::Control((*id).into(), Control::TxConflicts))
        }
        _ => Err(error("not found")),
    }
}
pub(crate) fn children(store: &Store, path: &str) -> Result<Option<Vec<String>>> {
    let parts: Vec<_> = path.split('/').filter(|p| !p.is_empty()).collect();
    let names: Vec<String> = match parts.as_slice() {
        [] => ["workspaces", "readonly", "snapshots", "control"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["control"] => ["workspaces", "snapshots", "transactions"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["workspaces"]
        | ["readonly"]
        | ["snapshots"]
        | ["control", "workspaces"]
        | ["control", "snapshots"] => store.names()?,
        ["control", "workspaces", _] => ["VERSION_MAP", "ACTIVE_VERSION_MAP", "STATUS"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["snapshots", n] | ["control", "snapshots", n] => store
            .snapshots(n)?
            .iter()
            .map(ToString::to_string)
            .collect(),
        ["control", "snapshots", _, _] => ["VERSION_MAP", "ACTIVE_VERSION_MAP", "readonly"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        ["control", "transactions"] => {
            let c = store.conn()?;
            let mut q = c.prepare("SELECT id FROM activations ORDER BY id")?;
            let rows = q
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        }
        ["control", "transactions", _] => {
            ["STATUS", "PROPOSED_VERSION_MAP", "conflicts.json", "source"]
                .into_iter()
                .map(str::to_string)
                .collect()
        }
        _ => return Ok(None),
    };
    Ok(Some(names))
}
pub fn workspace_path(mount: &std::path::Path, workspace: &str) -> std::path::PathBuf {
    mount.join("workspaces").join(workspace)
}
pub fn context(
    mount: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<(String, std::path::PathBuf)> {
    let cwd = std::fs::canonicalize(cwd)?;
    let mount = std::fs::canonicalize(mount)?;
    let relative = cwd
        .strip_prefix(&mount)
        .map_err(|_| error("outside a managed tree; specify --workspace"))?;
    let parts: Vec<_> = relative.iter().collect();
    let count = match parts.first().and_then(|s| s.to_str()) {
        Some("workspaces" | "readonly") if parts.len() >= 2 => 2,
        Some("snapshots") if parts.len() >= 3 => 3,
        _ => return Err(error("not in a workspace source tree")),
    };
    let workspace = parts[1]
        .to_str()
        .ok_or_else(|| error("non-UTF8 workspace"))?
        .to_string();
    Ok((
        workspace,
        mount.join(parts[..count].iter().collect::<std::path::PathBuf>()),
    ))
}
