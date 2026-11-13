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
  workspaces/<id>/layout.json         present only during layout recovery
  cache/                             disposable materialized base payloads
  tree-cache/v1/                     shared immutable commit/directory caches
  logs/activity.jsonl                 structured operation and startup activity
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

The optimized executable is `target/release/gtl`, or
`$CARGO_TARGET_DIR/release/gtl` when that variable is set. Put it on your `PATH`
for the commands below. Use the optimized build when evaluating latency, and
restart a running server after rebuilding to pick up changes.

Set `CARGO_TARGET_DIR` to keep build outputs outside the checkout if desired.
Test fixtures and demo artifacts live in ignored `.tmp/`. Tests use local Git
repositories and never publish to remote services. Mounted tests require actual
mount permission as well as the FUSE device. Git 2.43 is supported.

## Quick start: empty workspace to TheRock

If a server already owns your state, reuse it. Otherwise, start a foreground
server in one terminal (no service installation):

```sh
gtl daemon run
```

Keep it running and use another terminal in the same mount namespace:

```sh
gtl workspace init example
cd ~/ws/example
gtl mount add https://github.com/ROCm/TheRock.git --at . --submodules none
ls

# Expand direct submodules now, or choose recursive for all levels.
gtl mount submodules --at . --depth first-level
```

These examples use the default state and mount location. For a custom state, use
`--state-dir /absolute/path/to/state` consistently in both terminals and use the
workspace path reported by `workspace init`. See the service configuration below.

Initialization creates a managed bare depot, an empty `depot.toml` commit, and a
linked workspace checkout on its own branch. No remote, network fetch, or Git
author setup is needed for that bootstrap commit. The initial FUSE view contains
only discovery metadata; existing workspace names are never overwritten.

Mount changes activate immediately but remain uncommitted in the depot. Inspect
and checkpoint the layout when ready:

```sh
gtl status example
git -C .depotgit diff -- depot.toml
git -C .depotgit diff --cached
gtl depot edit example -- git add depot.toml
gtl checkpoint example -m 'Add TheRock layout'
```

Checkpoint needs your Git author identity and includes all staged changes, so
inspect the index first. `gtl diff example` describes source-overlay changes;
use the backing Git diff above to inspect the version map itself.

With the server stopped, `gtl --offline workspace init example` creates the
persistent state for mounting later. `--offline` bypasses the server; it does not
promise no network access, and it cannot run while the daemon owns that state.

## Agent task guides

The guides are embedded in the executable and need no server or state directory:

```sh
gtl agents                           # list task chapters
gtl agents workspace-create
gtl agents mount-add
gtl agents mount-remove
gtl agents mount-submodules
```

## Add a repository mount

From inside a composed workspace, add a repository at its root:

```sh
gtl mount add https://github.com/ROCm/TheRock.git --at . --submodules recursive
```

`--submodules none` (the default) leaves gitlinks as empty directories.
`first-level` imports direct submodules; `recursive` imports all levels.
All imported repositories are pinned to exact commits. Use `--revision` to
choose the root revision instead of remote HEAD.

The command discovers the current workspace/state, or accepts `--workspace NAME`
with an explicitly selected state. `--at` is always workspace-relative, even when
run from a subdirectory; `.` means the workspace root. Only mount commands infer
state from the current directory: retain your `--state-dir` flag or shell wrapper
for status, checkpoint, and other commands.

It activates the validated layout and modifies
`depot.toml` without staging or committing. Existing overlay files stay on top.
Read `gtl agents mount-add` for verification, checkpointing, and cancellation.

## Remove mounts and expand submodules

```sh
# Remove exactly one mount; descendants remain.
gtl mount remove --at .
# Remove that mount and every descendant mount.
gtl mount remove --at components/library --recursive

# Expand the currently pinned mount without advancing any revisions.
gtl mount submodules --at . --depth first-level
gtl mount submodules --at . --depth recursive
```

Removal preserves overlay files and rejects tombstones that would lose their
base provenance. Expansion adds missing submodules and reuses matching mounts;
conflicting origins, pins, or subtrees require a future resolve workflow. It reads
pinned `.gitmodules`, not local edits. Repeated expansion is harmless and never
prunes existing descendants. Both commands activate an uncommitted layout using
the same workspace discovery and cancellation behavior as `mount add`.
See `gtl agents mount-remove` and `gtl agents mount-submodules`.

To keep accessible submodules when another child's fetch fails:

```sh
gtl mount submodules --at . --depth recursive --skip-unavailable
# Also accepted by mount add with --submodules first-level or recursive.
```

Partial success returns `complete: false` and a `skipped` array containing the
path, origin, commit, error, and a copy/paste `retry_command` for every skipped
child. It exits successfully; scripts requiring a complete import must inspect
`complete`. Successful parents and siblings remain mounted; missing children can
be retried by repeating expansion. Cancellation, layout conflicts, invalid
metadata, local failures/timeouts, and root fetch failures remain fatal.

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
gitlinks require an explicit mount or the mount's
`uninitialized_submodules = true` policy, which exposes them as empty directories.

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

## Activity and cancellation

Foreground `gtl daemon run` prints timestamped activity to stderr when attached
to a terminal. Interactive CLI commands also show operation progress while waiting.
Redirected commands keep stdout as JSON and do not print progress to stderr.
The server and offline commands always record activity in
`STATE/logs/activity.jsonl` (10 MiB per file, three rotated archives). Detached
startup diagnostics remain in `STATE/logs/daemon.log`; systemd captures stderr
in its journal.

```sh
gtl activity                         # IDs, stages, elapsed times; no workspace lock
gtl cancel 42                        # cancel an operation by its reported ID
tail -f /path/to/state/logs/activity.jsonl
```

Startup reports journal recovery, mount-root preparation, tree-cache statistics,
overlay validation, FUSE attachment, and total time to readiness.
Long startup phases emit five-second heartbeats.

Activity includes directory demand/lookahead loads, repository fetches, Git transfer progress,
payload/LFS hydration, elapsed-time heartbeats, failures and cancellation.
Raw `GIT_TRACE2_EVENT` remains available for deeper Git diagnostics.

Background Git commands run without a controlling terminal and with Git/SSH
askpass prompts disabled. Unknown SSH hosts and missing credentials therefore
fail instead of waiting for input; configured SSH commands and host-key policies
are preserved. Failed fetches print a shell-quoted interactive retry command.
Run it in your own terminal after the operation finishes, handle host verification
or authentication there, then retry Gitalator. No host keys are automatically
accepted or verification checks disabled.

Ctrl-C on a blocked filesystem client propagates FUSE interruption through queued
work, lock waits, and backend subprocess groups. Ctrl-C on a CLI command closes its
control connection and cancels the corresponding server operation. Offline commands
and server shutdown also cancel backend work. A published namespace or layout journal is
finished before cancellation takes effect, preserving recoverability; cancellation
does not roll back already completed changes or delete successfully fetched objects.
A cancelled backing-Git edit/checkpoint may leave an edit lease requiring repair.

Base files and directories report the pinned mount commit's **committer time**.
These are stable snapshot timestamps, not per-file last-change dates. Overlay
files retain their real worktree timestamps.

## Edit, inspect, and commit

```sh
gtl status dev
gtl validate dev                     # mount boundaries and depot/overlay invariants
gtl validate dev --full              # also traverse the complete base namespace
gtl dir dev                          # report the composed workspace path
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
with FUSE access. Manual pin/layout changes and workspace branch changes are
rejected: restore those before ending the edit session. Use `mount add`,
`mount remove`, and `mount submodules` for supported layout changes.

## Fork, clone, and remove workspaces

```sh
gtl workspace fork clean --from dev   # committed state only, new branch/index
gtl workspace clone experiment --from dev
# Live clone preserves staged, unstaged and nonignored untracked source files.
# Ignored build outputs are excluded; neither workspace is committed implicitly.
gtl workspace list
cd ~                                 # leave the workspace before removing it
gtl workspace remove clean           # refuses dirty/untracked/ignored files
```

Workspace removal requires a quiescent workspace with no active edit lease or
open file/directory handles or mappings. It refuses staged or unstaged changes,
untracked files, and ignored outputs. Checkpoint changes you want to retain and
remove unwanted outputs explicitly; there is no force-removal option. Mount
operations dirty `depot.toml`, so a workspace with newly changed mounts must be
checkpointed before removal.

Removal deletes the workspace checkout/registration but retains its depot branch,
history, and shared repository stores. It does not stop the shared daemon or
remove other workspaces. This differs from `mount remove`, which keeps the
workspace and its overlay files.

Normal workspaces
use separate local `workspaces/<id>` branches; fetching cannot overwrite them.
All generated discovery symlinks are absolute and work through workspace aliases.
Metadata JSON is versioned and reports the active generation and exact mappings.

## Startup, hydration, and latency

Component fetches use Git's `blob:none` filter. Startup prepares mount roots and
validates mount boundaries, overlay paths, and tombstone provenance. It does not
recursively index every component file. Directory contents are discovered on
lookup or listing, and file payloads remain lazy.

Immutable directory entries are shared by repository identity and tree OID,
across mounts, pins, and workspaces. Commit/root metadata and directory objects
persist under `STATE/tree-cache/v1/`; malformed caches rebuild from Git. The
in-memory directory cache uses LRU eviction with a 128 MiB default accounted
budget. Active requests and oversized directories can temporarily exceed it.
Traversal history is bounded too; scanning does not retain every visited path.

```sh
GTL_TREE_CACHE_MB=256 GTL_INDEX_JOBS=8 gtl daemon run
```

`GTL_TREE_CACHE_MB` accepts 1–65536 MiB. `GTL_INDEX_JOBS` accepts 1–64, defaulting
to at most eight available CPUs, and controls parallel root preparation and full
validation. It does not make submodule import parallel. `gtl status NAME` includes
shared tree-cache load/hit counts and resident/budget bytes.

Normal validation is targeted. An unsupported entry in an untouched directory
may therefore be discovered only on access; the affected operation returns an
error and activity logs identify its mount and path. Run `gtl validate NAME --full`
for a cancellable full base-namespace check and cache warmup without fetching file
payloads. Fetching pins during explicit import, enumerating the submodule graph,
and materializing a directory for rename are still explicit work.

The eager flattened index implementation has been removed. Existing manifests
and workspace records remain compatible. Old `STATE/indexes/` files are ignored;
remove them with the server stopped if you want to reclaim space. Both old index
files and the new tree cache are disposable.

Reproduce startup, first access, `ls -l`, full-traversal, Git-command-count, and
memory measurements on a local 679-mount fixture with small and larger trees:

```sh
python3 scripts/bench-namespace.py --before /path/to/eager/gtl \
  --after /path/to/release/gtl --output /workspace/scratch/namespace-bench
```

The benchmark uses already-fetched local repositories and temporary foreground
servers, preserving traces and results under its output directory. It isolates
namespace costs, not remote network performance.

One optimized-build run with 679 mounts and eight local origins measured:

| Metric | Eager index | Lazy namespace |
|---|---:|---:|
| Cold startup, 64 files/mount | 9.38 s | 0.23 s |
| Cold startup, 1,024 files/mount | 41.77 s | 0.23 s |
| Startup resident memory, larger fixture | 360 MiB | 8 MiB |
| First directory listing, larger fixture | 0.006 s | 0.039 s |
| `ls -l`, larger fixture | 0.357 s | 0.204 s |
| Full walk, larger fixture | 72.41 s | 66.72 s |
| Cached startup, larger fixture | 1.20 s | 0.04 s |

Both walks returned identical file counts. Startup Git invocations fell from
4,753 to 40 in this fixture; the lazy full walk issued 15 additional Git commands.
The shared tree cache held about 1.6 MiB after the larger walk. Total daemon memory
also includes FUSE inode/handle state, blob metadata, and active requests: the
128 MiB tree-cache budget is not a total process-memory limit.

Name-only enumeration does not fetch blobs. Attribute lookups (`ls -l`, build
scanners) resolve sizes in batches, without creating extracted payload files.
LFS pointer sizes do not require downloading LFS payloads. Actual reads and
copy-up still hydrate file contents.

The first attribute request after a directory listing resolves up to 256 objects.
Evidence of recursive traversal expands this to at most 1,024 objects across 32
nearby directories within the same mount. Tree lookahead batches immediate
directory objects and never recursively expands itself. Repeated mounts/workspaces share object
metadata. Demand work precedes queued lookahead; failures fall back to the
requested file and suppress further speculation for that directory.

An isolated release-build benchmark of 600 2-KiB files measured:

| Metric | Before attribute batching | With attribute batching |
|---|---:|---:|
| Cold `ls -l` | 22.62 s | 0.66 s |
| Warm `ls -l` | 0.32 s | 0.32 s |
| Git `cat-file` invocations in cold listing | 1,202 | 3 |
| Network fetches | 3 | 3 |
| Extracted payload files | 600 | 0 |

Both use local blob-filtered origins and optimized binaries, with Git tracing
enabled. This isolates serialized subprocess/extraction overhead; it is not a
TheRock/network-performance estimate. Run the same fixture with:

```sh
python3 scripts/bench-attributes.py --before /path/to/previous/gtl \
  --after /path/to/release/gtl --output /workspace/scratch/attributes-bench
```

The script starts temporary foreground FUSE servers, stops them afterwards, and
retains stores, traces and results under the output directory.

## Foundation boundaries

The old POC's SQLite storage, snapshots, activation/resolution, export/import,
and review/landing workflows have been removed. The current layout commands
support addition, removal, and expansion at existing pins. Old state is rejected
without being deleted or migrated. Use a fresh state root. Pin advancement, branch switching,
semantic merging, and component submission will be implemented as new workflows.

See [filesystem and recovery contracts](docs/architecture.md) for supported
operations, durability, and current resource limits.
