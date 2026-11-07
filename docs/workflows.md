# Commands and workflows

All commands accept `--state-dir PATH` and `--json`. Without `--json`, output is
pretty JSON, except `new-workspace` and `dir`, which print paths; with it, compact JSON. `gtl COMMAND --help` describes arguments.
Examples below assume the same state directory for every invocation.

## Workspace creation and navigation

Start explicitly with `daemon start`, `daemon start --detached`, or `daemon run`.
See the [quick start](../README.md). `new-workspace NAME --from URL --name TOP`
includes the super-repository and immediate submodules by default. `--like NAME`
copies active exact pins with clean edits/draft. `--clone NAME` atomically copies
active pins, edits, and even an invalid staged draft; it refuses active activation.
Neither copies snapshot history. The destination appears only after complete
resolution and validation; failed creation may still populate shared caches.

`submodules expand [MOUNT_PATH...] [--workspace NAME] [--recursive]` reads committed
`.gitmodules` and gitlinks from existing pins, resolves relative URLs, and adds
missing child mounts atomically. Paths are workspace-relative repository mount
paths. Without paths, all current mounts are considered. Without `--recursive`,
only their immediate children are added; repeated calls hydrate deeper levels.
Existing pins never refresh. Expansion requires whole-repository mounts. Expansion rejects staged map changes, activation,
conflicting existing mounts, recursive cycles, and edits beneath new mount cones.
If the workspace changes while fetching, publication fails and can be retried.

`dir top [--workspace NAME]` prints the live workspace root. `dir parent` prints
one level up, bounded at the current source view root. Workspace inference resolves
symlinks and supports live, readonly, and snapshot views. Control directories are
not source contexts. Use `cd "$(gtl dir top)"` and `cd "$(gtl dir parent)"` in a shell.
Submodule expansion also infers the workspace when run inside a managed source tree.

## Layouts and source management

Repository IDs are local stable aliases for remote URLs. A separate storage key
hashes the canonical origin, so aliases of the same origin share bare objects and
repository locks. Normalization is deliberately conservative: local paths resolve
symlinks, URL hosts normalize case, and GitHub URL trailing `.git` is optional;
transport, SSH aliases, and path case are not merged. Credential-bearing URLs are
rejected; use credential helpers. Mount IDs are stable
ownership identities: retain an ID when moving its cone; use a new ID for a new
owner. The repository registry is outside VERSION_MAP so layout history need not
contain credentials or transport aliases.

```toml
format = 1
[[mount]]
id = "platform"
repo = "platform"
path = ""                  # root cone
revision = "main"          # branch, refs/heads/name, refs/tags/name, or full OID

[[mount]]
id = "firmware"
repo = "restricted-fw"
path = "src/firmware"      # replaces this entire parent directory cone
subtree = "firmware"       # optional subtree inside the backing repository
revision = "refs/tags/v1"
```

Deepest directory mount wins. Files never interleave at the same path. Parent
content beneath a nested mount stays hidden even if a child file is deleted.
Mount roots/ancestors cannot be removed or renamed through source directories. Mounting over
a backing file or symlink is rejected. Nested directory shadowing is permitted;
a detailed shadow report remains backlog work.

`repo add ID URL`, `repo list`, and `repo fetch ID` manage backing repositories.
Configure SSH aliases/agents and Git credentials normally; fetches never prompt.
Use credential-free URLs. This store runs as one user, not as an ACL boundary
between different users. Separate stores/accounts are required for that boundary.

`workspace create NAME --map FILE` creates an empty active layout with a staged
map. `activate NAME` resolves it. `workspace create NEW --from OLD [--snapshot N]`
forks workspace state without materializing a full copy. `workspace list` lists views.

`map show NAME [--active]` reads maps; `map set NAME FILE` stages a proposal.
`map import-superrepo NAME PATH [--revision HEAD]` imports committed `.gitmodules`
and gitlinks. It does not import the index or uncommitted super-repo edits, or
recursively discover nested submodules. Inspect imported URLs before activating.

Activations record resolved maps on workspace-specific branches in the local
`STATE/layouts` Git repository. `layout branch NAME LABEL` saves the active map;
`layout switch NAME LABEL` stages its exact pins. Explicit `activate` is still
required after switching. Layout branches are local and are not pushed anywhere.

## Activation and resolve

Editing VERSION_MAP is only editing a proposal. `activate NAME --plan` fetches
and resolves references without changing the active view (it can take time and
populate caches). Activation defaults to selective, blob-lazy fetching: only the
requested branches/tags or missing exact commit IDs are fetched. Cached exact
pins skip the remote fetch; named refs are refreshed on every activation. Git
servers must allow fetching the requested commit and support blob filtering to
avoid transferring file contents. Git reports filter fallback in the progress
output. Commit history and tree metadata are still fetched and enumerated to
validate the layout; this is not a shallow clone or fully deferred tree loading.
File/LFS contents are retrieved when needed (including reads during edit rebases).
`repo fetch ID` explicitly refreshes all branches and tags.

Fetch status and Git transfer progress appear on stderr, prefixed with the
repository alias, mount path(s), and shortened origin, both offline and through a
mounted daemon (for example `repo-ab12 [compiler/amd-llvm · ROCm/llvm-project]`).
The root mount is shown as `/`; URL credentials, query strings and fragments are
omitted from labels. Stdout remains the
final JSON result, including with `--json`. Progress includes pending, lock wait,
fetching/cached, tree indexing, ready, and fetch failures. Restart a daemon from
an older binary before using the new client's progress protocol. `activate NAME` prepares a candidate, rebases local edits, and
publishes automatically if there are no conflicts.

During a transaction, old source reads remain available; writes are frozen.
VERSION_MAP remains editable, including while resolving a previous proposal.
Later draft edits do not change that transaction's candidate map.

```sh
gtl conflicts TRANSACTION
# Edit mount/control/transactions/TRANSACTION/source/<path> to resolve contents.
gtl resolve TRANSACTION MOUNT_ID RELATIVE_PATH
gtl activate WORKSPACE --continue
# Or retain old active state and discard the proposed activation:
gtl activate WORKSPACE --abort
```

Text conflicts use Git's three-way merge. Binary, deletion, mode, ownership,
removed-mount and hidden-path conflicts require an explicit decision. For changes
whose ownership vanished, `resolve ... --discard` removes the candidate edit;
the original remains recoverable from snapshots. Merely editing markers does not
mark a conflict resolved. Failed preparation remains frozen until aborted; see
operations documentation for interruption recovery.

## Snapshots and stash

`snapshot create NAME`, `snapshot list NAME`, `snapshot pin NAME N`, and
`restore NAME N` operate on complete workspace checkpoints, including the draft
map. `stash NAME` checkpoints then resets the overlay to the current base; it
preserves the draft. Restore the saved snapshot to recover the stash.

Snapshots are immutable and workspace-numbered. Pin metadata exists, but currently
**all** snapshots/history are retained regardless of pins. `gc` only evicts fetched
payloads; it never expires your edits or numbered snapshots.

## Export, review, import, and persistence

`export NAME DESTINATION` captures a checkpoint and creates an ordinary Git clone
and local commit for each edited mount. DESTINATION must not exist. Each clone
contains its complete backing repository, including content outside the mounted
subtree, and points `origin` at the configured remote. Edits are limited to that
mount's owned cone. A manifest records exact bases and export heads. Empty
directories are local workspace state; Git cannot persist them.

Exports use a synthetic Gitalator author. Amend/change authorship in the clone
if needed, then import/re-export to obtain a new recorded review checkpoint.
Two edited mounts of the same repository are currently rejected in one export;
combine them or use separate workspaces. Export materializes repositories and
may fetch LFS objects. A failed partial export is retained for inspection; retry
at a new destination.

```sh
gtl export dev /absolute/path/review-001
# Build, edit, commit, or review using normal Git tools in review-001/<mount>/.
gtl import dev /absolute/path/review-001
```

Import compares current export worktree contents (including commits and untracked
files) against the recorded exported head, then three-way merges that delta into
the current workspace. It uses the same freeze/candidate/resolve protocol as
activation. Changes outside the original cone or crossing a nested owner fail.
An import does not automatically persist anything to a remote.

`bundle EXPORT DESTINATION` produces verified Git bundles and separate verified
LFS payloads plus a portable manifest. Missing historical LFS payloads fail
explicitly: fetch them in the export clone with `git lfs fetch --all` and retry at
a new destination. To restore, clone each `.bundle`; copy its corresponding
`<mount>.lfs/` contents into `.git/lfs/objects/` and run `git lfs checkout`.
There is no automated portable-bundle import command yet.

`review EXPORT` plans only. `review EXPORT --publish [--base BRANCH]` explicitly
pushes checkpoint branches/LFS payloads and creates or reconciles per-silo GitHub
PRs using `gh`. Branch revisions supply a default base; exact commit/tag exports
require `--base`. It rejects dirty or moved export heads. Remote adapters have
**not** been tested against real services in this draft; use normal Git/gh review
flows until that integration testing is done.

Merge using each silo's normal review policy. `land EXPORT --verify-only` checks
recorded PR heads and exact landed-tree equality, then writes a receipt. Unrelated
base changes may cause conservative rejection. Automatic multi-repository landing,
CI attestations, and promotion of the new pins into VERSION_MAP are deferred.
There is no atomic cross-repository push/merge guarantee.
