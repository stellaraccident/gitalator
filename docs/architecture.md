# Depot foundation contracts

The depot's committed `depot.toml`, `overlay/`, and `tombstones/` define portable
source state. JSON runtime records identify each worktree, branch, active map,
and generation. Runtime records and materialized base caches are not source
history. No SQLite database or secondary version-map repository exists.

## Composition

The deepest mount owns its directory cone, including absent entries. Upper files
replace lower files; directories merge. A deletion marker hides a lower entry and
its descendants. An opaque marker hides the lower subtree while exposing upper
children. Upper symlinks retain their source targets. Generated `.depotgit` and
`.mountgit` links instead use absolute backing paths.

A marker is a regular JSON file at `tombstones/<virtual-path>`:

```json
{
  "format": 1,
  "kind": "delete",
  "mount_id": "code",
  "base_commit": "<full commit>",
  "base_oid": "<entry object>",
  "base_kind": "file"
}
```

Kinds are `delete` and `opaque`; base kinds are `file`, `directory`, and `symlink`.
Directory deletion replaces redundant descendant markers. Recreating a deleted
directory produces opacity, so old children do not reappear. Deleting an upper
child of an opaque directory needs no descendant marker. Provenance mismatches
and payload/marker contradictions are validation errors, never ignored records.
Reverting a child hidden by an ancestor tombstone is rejected: revert the ancestor
first rather than silently deleting the upper child while keeping it hidden.
Mount roots and their ancestor scaffolding cannot be deleted or renamed through
FUSE. Changing layout is outside the foundation campaign.

## Physical mutation and recovery

Normal writes use real upper file descriptors. Copy-up stages data and mode before
publication. Namespace operations publish a durable redo journal before changing
payload/tombstone paths. Recovery completes that operation before serving source
files. Redo distinguishes a completed rename from one whose source still exists;
replaying it cannot delete its completed destination. Durable journals are retired
only after affected parent directories are synced. A failed operation with a live
journal blocks subsequent mutations until restart/recovery.

Ordinary dirty bytes have normal filesystem write/fsync durability, not automatic
Git commits. `flush` and `fsync` sync the backing file. Open descriptors retain
underlying inode identity across unlink and rename replacement. Copy-up redirects
existing handles to the upper inode rather than making immutable caches writable.
User processes must not write directly into backing files outside a coordinated
edit session; watching directories cannot provide atomic Git checkout semantics.

Workspace creation records an initialization intent before materializing the
linked worktree. Restart completes an interrupted creation or leaves it explicitly
suspended for repair; it does not delete the source workspace or its branch.

Edit leases persist across crashes. Their token, active map, and workspace branch
are validated on completion. A checkpoint crash after Git commit leaves a lease
for explicit inspection and completion; it never silently discards dirty files.
Git refs and filesystem files do not participate in one native transaction.

Checkpoint exports the staged index to an isolated physical candidate for
validation, then normalizes equal base overrides. It restores the original index
if Git rejects a commit. Normalization never copies unstaged bytes into the index.
A live clone copies the index separately from working files to preserve that same
distinction; ignored files are deliberately excluded. Physical `.git` pointer
files are created by Git, never copied from another worktree.

## Discovery and cache coherence

`.depotstate.json` at each workspace root contains the active map, workspace ID,
branch, depot repository/worktree locations, generation and edit status.
`.mountstate.json` adds mount ID, repository identity, exact commit, subtree,
virtual path, and Git object format. `.mountcommit` is a full object ID plus newline.
Each metadata handle holds one serialized value. Readers of multiple files must
compare generation tokens and retry around a change. This detects layout changes,
not simultaneous edits to source contents; content analysis needs a checkpoint or
edit session. Discovery JSON contains no credential-bearing remote URLs.

FUSE uses cached I/O for mmap and executable loading, zero attribute/entry TTLs,
and explicit invalidation after coordinated backing operations. Source operations
in a suspended workspace fail with EBUSY. Other workspaces remain available.
Open files, directory streams, and mappings prevent acquiring an edit session.
A stale open racing generation publication is rejected.

## Supported baseline and limits

Regular files, symlinks, directories, executable modes, timestamps, rename/unlink,
read/write/truncate, mmap, and executable loading are supported. Kernel-local
advisory locking is used. Tests compile/link/run C code through FUSE and exercise
shared writable mmap and common atomic-editor-save patterns. Ownership changes,
hardlinks, device nodes, ACLs, arbitrary xattrs, and non-UTF8 filenames are not part
of this release. Empty upper directories are runtime filesystem state; Git does
not preserve them in commits. Changing metadata on an unlinked read-only base
handle is unsupported; it must never mutate a shared immutable cache.

Component trees are discovered one directory at a time. The foundation uses
exact-commit Git fetches with `--filter=blob:none`; origins must support filtering
to avoid eager transfer. Directory enumeration uses tree metadata without fetching
blobs. Attribute lookup fetches missing Git blobs in bounded batches, caches their
sizes, and inspects blobs of at most 1 KiB together with `cat-file --batch`.
Canonical LFS pointer sizes are returned without LFS downloads. Symlink sizes
describe their literal link text. Attribute lookup does not extract payload files.

A listing followed by attributes enables up to 256 objects from that directory.
If its parent was also listed, the batch can span up to 1,024 objects across 32
immediate child/sibling directories within the same mount. Selection starts near
the requested path and wraps; resolved objects, upper overrides and tombstoned
paths are skipped. Shared immediate-tree caches avoid whole-namespace scans.
Failure falls back to the requested object and disables speculation for that
directory until workspace reload. Cancellation does not disable speculation.

Object metadata is shared by repository ID and object ID across all workspaces
in one store. A cancellable per-repository gate coalesces concurrent fills and
prioritizes queued demand over queued lookahead; running batches finish normally.
Metadata and small-blob bodies are runtime caches and are rebuilt in batches after
restart. Actual opens/reads/copy-up extract payloads on demand; cached small
bodies avoid another extraction subprocess. Local object probes
never fetch; local presence checks binary-search pack indexes and inspect loose-object paths
before invoking Git. This avoids Git 2.43's missing-promisor history walk.
Version 1/2 pack indexes, SHA-1/SHA-256 IDs, and object alternates are supported.
Base blobs are materialized into a disposable cache, with Git LFS expansion and
payload verification. Depot checkout materialization hydrates its own tracked files.
Whole-blob memory use and oversized individual directories remain limitations. No automatic cache GC
is implemented. The component commits and LFS objects must remain available from
their origins for reconstruction on another machine.

One shared user daemon owns the FUSE session and local control socket. The socket
checks peer UID and lives in a private state directory. Systemd units are generated
for explicit startup, never enabled automatically. Stopping the daemon affects
all workspaces; an edit lease affects only its own workspace. No remote publishing
or implicit background synchronization occurs.

FUSE interruption is dispatched by the narrowly patched vendored fuser 0.14
dependency. Request IDs own cancellation tokens and reply ownership. Queued
requests can reply EINTR immediately without waiting for a worker; running
requests check cancellation during mutex/flock waits, traversal and backend work.
The dispatch thread never executes slow fallback work on queue saturation.
Cleanup requests have a separate worker; shutdown cancels outstanding work and
joins workers before releasing state ownership.

Git subprocesses run in separate process groups. Cancellation sends TERM, allows
a short cleanup grace, then kills remaining descendants and drains their pipes.
Missing-object probes use local pack indexes rather than promisor traversal.
A committed namespace journal is a non-cancellable section until redo completes.
Partial fetches may leave reusable Git objects; a cancellation is not a rollback.
Checkpoints and backing Git commands retain their existing edit-lease recovery
semantics if interrupted.

The activity registry has no dependency on workspace locks. Control protocol 5
streams progress frames before its final response; interactive clients render
them on stderr and redirected clients retain JSON stdout. Disconnecting a client
cancels its associated operation. The foreground daemon renders activity on a
terminal and all server/offline activity is persisted as rotating JSONL logs.
Three 10-MiB archives plus the current log bound retention. Credential-bearing
URLs and token/password fields are redacted from messages.

Immutable base entries use their mount pin's committer timestamp (including
subtree mounts), independent of object download/cache creation time. Overlay
entries use physical worktree metadata; generated discovery entries use workspace
state metadata timestamps.

`workspace init NAME` bootstraps an independent managed bare depot with a
manifest-only initial commit and no origin remote. The bootstrap identity is
scoped to that commit, leaving normal Git identity configuration untouched.
It then uses the same linked-worktree registration and recovery path as creation
from a remote. Fork/clone reuse the source's managed bare store directly, whether
or not it has an origin. The `agents` task chapters are embedded Markdown and
do not open state or contact the server.

Additive `mount add` prepares a new map outside the workspace lock and without
holding the global store lock. Activation rechecks identity/generation,
quiescence, physical manifest and overlay/tombstone compatibility. It refuses
existing mount destinations and replacement of populated mounted subtrees.
Imported submodules use exact gitlink pins, with explicit none/first-level/
recursive traversal. Unimported gitlinks are empty directories only when the
corresponding mount records `uninitialized_submodules = true`.

A per-workspace `layout.json` redo journal makes physical manifest and active
record updates restart-safe. Published activation finishes before cancellation;
preparation cancellation changes neither active layout nor manifest. The index
and HEAD are untouched, so the user explicitly stages/checkpoints the layout.
Live clones inherit active uncommitted layout; clean forks use committed layout.
The import limits are 32 nested levels and 4,096 total mounts.

Daemon startup prepares immutable commit/root descriptors in per-repository
batches, resolves explicit subtrees, and compiles a mount-path trie. This trie
serves ownership, synthetic ancestor directories, metadata boundaries, and nested
mount precedence without scanning all mounts for each path. No flattened
workspace index is built, copied during clone, or persisted.

The lazy namespace exposes fallible lookup and immediate children operations.
Workspace operations, FUSE directory kinds, staged checkpoint normalization, and
tombstone provenance checks use these operations. Listing never treats backend
errors as missing entries. Raw tree parsing supports SHA-1 and SHA-256 IDs and
rejects unsupported names/modes when the affected directory is loaded.

`STATE/tree-cache/v1/` contains disposable commit descriptors and checksummed
immediate-directory caches, published atomically. Directory identity is repository
ID plus tree OID; mount IDs, paths, timestamps and gitlink policy stay outside the
shared cache. A 128 MiB accounted LRU memory budget is adjustable with
`GTL_TREE_CACHE_MB=1..65536`. Active Arc references and oversized demand results
can temporarily exceed the budget; oversized trees are not retained in memory.
Per-workspace listing and failed-speculation hints each retain at most 4096 paths.

Per-repository cancellable priority gates coalesce tree fills. Cache locks are
never held during Git I/O or waits. Queued demand precedes speculative batches;
a cancelled loader releases its gate so other requests can retry. Traversal
lookahead reads at most 32 known immediate directories using batched tree-object
reads; it stays within the current mount and never expands itself recursively.
Failures retry the demanded directory and disable that context's speculation.

Startup and layout/edit/checkpoint activation validate mount boundaries and the
physical overlay/tombstone dependencies. Untouched component subtrees are not
walked. `validate NAME --full` additionally traverses every composed base mount,
skipping parent content replaced by nested mounts, and warms directory caches
without hydrating file payloads. Invalid untouched directories surface on access
or full validation, with mount/path context in activity logs. Existing formats
and layout journal recovery are unchanged. Legacy `STATE/indexes/` is ignored;
users may delete it while stopped. Old eager index code is removed entirely.

Startup reports root preparation, tree-cache statistics, overlay validation,
FUSE attachment, elapsed workspace time and readiness. Five-second heartbeats
cover long phases; `status` exposes shared tree-cache counters and memory usage.

Mount removal and pinned submodule expansion share the layout preparation and
activation path with addition. They snapshot the active map and generation, do
Git preparation without holding the workspace/global store locks, then recheck
quiescence, generation, and overlay/tombstone validity before journal publication.
Removal preserves physical overlay payloads; tombstones invalidated by changing
base ownership reject the transaction. Only an explicit recursive removal deletes
all mount declarations in a cone. Parent mounts permit uninitialized gitlinks
when child mounts are removed; unused repository declarations are pruned but bare
stores and object caches remain.

Submodule expansion reads the selected commit's gitlinks and repository-root
`.gitmodules` (restricted to the selected subtree where applicable). Matching
existing mounts retain IDs and pins. Missing mounts are imported with exact
pins; conflicting URLs, pins, or subtrees reject the complete operation. Depth
is additive: first-level never de-expands children. Unchanged maps do not rewrite
manifest or generation. No local overlay configuration, revision updates, pruning,
or patch rebasing are inferred from expansion.


Background Git subprocesses create a new session (also their cancellable process
group), preventing SSH from reopening the daemon's controlling terminal.
`GIT_TERMINAL_PROMPT=0`, disabled Git/SSH askpass helpers, and
`SSH_ASKPASS_REQUIRE=never` prevent background credential dialogs. Custom
`GIT_SSH_COMMAND`, `GIT_SSH`, and `core.sshCommand` selection remains Git's own;
Gitalator does not change host-key trust settings. Fetch errors retain a
shell-quoted interactive retry command for the managed bare repository.

Opt-in `--skip-unavailable` applies to child fetch process failures during import
and expansion. Each child's map/recursion-stack preparation is a savepoint. A
failed fetch restores that savepoint, records its diagnostic/retry command, and
continues siblings. Nested skipped children keep their successful parents. The
owning mount permits uninitialized gitlinks for omitted children. The final
candidate is still validated and journaled atomically. Cancellation, metadata
errors, layout conflicts, process/storage errors, and timeouts are not swallowed.
Skipped results are returned separately from map changes (`complete`, `skipped`);
repeating expansion retries omissions without repinning successful mounts.


Root preparation and explicit full validation use bounded parallel windows,
defaulting to min(available CPUs, 8), overridable with `GTL_INDEX_JOBS=1..64`.
Workers inherit activity/cancellation, and repository creation/fetch retains its
existing locks. Cancellation or failure prevents candidate layout activation;
complete immutable directory caches may remain reusable. Submodule discovery and
explicit recursive materialization retain their existing traversal semantics.
