# Gitalator (`gtl`)

A depot-backed federated monorepo for Linux. One user daemon mounts all workspaces
at `~/ws/<name>`. A depot Git repository versions the complete mount map plus a
sparse overlay of changes across independent, pinned component repositories.

The physical depot worktree and the FUSE source tree are separate representations:

```text
~/.gitalator_state/
  repos/<identity>/                   shared bare depot/component repositories
  workspaces/<id>/depot/              one linked depot worktree per workspace
    .git                             linked-worktree administrative pointer
    depot.toml                       exact component pins and layout
    overlay/<virtual-path>           replacement or newly created source files
    tombstones/<virtual-path>        deletion/opacity records
  workspaces/<id>/workspace.json      active map, generation, branch, edit lease
  workspaces/<id>/journal.json        present only during namespace recovery
  cache/                             disposable materialized base payloads
  config.json                        service mount location
  control.sock
~/ws/dev/                            normalized composed source tree
  .depotgit -> /absolute/.../depot/
  .depotstate.json
  src/
    .mountgit -> /absolute/.../repos/<identity>/
    .mountcommit
    .mountstate.json
    ...
```

Unchanged files come from exact component commits. Writes copy up to real files in
`overlay/`; they do not go into a database. Reads never create overrides. Deletions
are ordinary Git-tracked JSON marker files in `tombstones/`. Several mounts may
share a repository while using different pins and independent edits.

## Build and test

Requires Rust/Cargo, a C linker, Git, Linux `/dev/fuse`, and `fusermount3`. Git LFS
is needed for LFS content and its integration test. Mounted build tests also use
`cc` and Python 3. No SQLite or libfuse development package is needed.

```sh
cargo build --release --locked
scripts/check.sh              # formatting, Clippy, default tests
scripts/check.sh --fuse       # also mounted tests and a local demo
```

Set `CARGO_TARGET_DIR` to keep build outputs outside the checkout if desired.
Test fixtures and demo artifacts live in ignored `.tmp/`. Tests use local Git
repositories and never publish to remote services. Mounted tests require actual
mount permission as well as the FUSE device. Git 2.43 is supported.

## Create a depot

Create an ordinary Git repository containing a committed `depot.toml`:

```toml
format = 1

[repositories.code]
url = "/absolute/path/to/component-repository"

[[mounts]]
id = "code"
repository = "code"
commit = "<full commit object ID>"
path = "src"
subtree = ""
```

`overlay/` and `tombstones/` may initially be absent. Add depot-owned source files
under `overlay/` at their virtual paths. Repository URLs must be credential-free;
authentication belongs to Git's external credential/SSH configuration. Local
component paths must be absolute. Nested mounts require explicit map entries;
gitlinks are accepted only where an explicit mount supplies that path.

```sh
gtl daemon start                     # one systemd user service for all workspaces
# Without a user manager, explicitly choose: gtl daemon start --detached
gtl workspace create dev --from /path/to/depot --revision main
cd ~/ws/dev
```

The service is started explicitly; commands never start it implicitly. The unit
is not enabled at login automatically. `gtl daemon run` runs in the foreground.
For isolated stores use `--state-dir /absolute/path` on every invocation; their
default mount is `STATE/mount`. With the daemon stopped, change its mount with:

```sh
gtl --offline config --mount "$HOME/ws"
```

Changing the path requires a restart. Normal `daemon stop` reports busy mounts;
`daemon stop --lazy` explicitly detaches. Backing state is not relocated.

## Edit, inspect, and commit

```sh
gtl status dev
gtl diff dev                         # semantic changes relative to component pins
git -C ~/ws/dev/.depotgit status      # actual sparse depot Git status
gtl revert dev src/file.cc           # remove override/tombstone and reveal base
```

Read-only Git inspection is safe while mounted. Run backing mutations through an
edit session, which refuses open files/directories/mappings and suspends just that
workspace until validation completes:

```sh
gtl depot edit dev -- git add overlay/src/file.cc tombstones/src/deleted.cc
gtl checkpoint dev -m 'Update compiler integration'
```

Checkpoint validates the **staged tree**, commits only staged changes, and removes
staged overrides identical to their base. Unstaged bytes remain intact. If
normalization leaves no tree change, it succeeds without an empty commit.
Generated outputs use the same upper tree. Ignore patterns in the physical depot
must address physical paths, for example `overlay/out/`; component `.gitignore`
files do not automatically become depot ignore rules. Never stage the FUSE union
by setting `GIT_DIR` while using the virtual tree as a worktree.

For multi-command tools, `gtl depot begin dev` returns a token and physical worktree;
finish with `gtl depot end dev TOKEN`. Failed commands or validation leave the
workspace suspended. `gtl status dev` reports the token for repair after a crash.
Do not perform raw checkout/reset/rebase or modify physical payloads concurrently
with FUSE access. Changing the active pins/layout or workspace branch is rejected
in this foundation release: restore those before ending the edit session.

```sh
gtl workspace fork clean --from dev   # committed state only, new branch/index
gtl workspace clone experiment --from dev
# Live clone preserves staged, unstaged and nonignored untracked source files.
# Ignored build outputs are excluded; neither workspace is committed implicitly.
gtl workspace list
gtl workspace remove clean           # refuses dirty/untracked/ignored files
```

Removal deregisters the worktree but retains its branch/history. Normal workspaces
use separate local `workspaces/<id>` branches; fetching cannot overwrite them.
All generated discovery symlinks are absolute and work through workspace aliases.
Metadata JSON is versioned and reports the active generation and exact mappings.

## Foundation boundaries

The old SQLite storage, snapshots, activation/resolution, export/import, and
review/landing workflows have been removed. Old state is rejected without being
deleted or migrated. Use a fresh state root. Pin advancement, branch switching,
semantic merging, and component submission will be implemented as new workflows.

See [filesystem and recovery contracts](docs/architecture.md) for supported
operations, durability, and current resource limits.
