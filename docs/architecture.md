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

Component trees are indexed in memory at workspace load. The foundation uses
exact-commit Git fetches with `--filter=blob:none`; origins must support filtering
to avoid eager transfer. Directory enumeration uses tree metadata without fetching
blobs. Payload access fetches missing object IDs explicitly under a shared repository
lock, batching up to 256 direct siblings after their parent has been listed.
Failed speculative batches fall back to the requested object. Local object probes
never fetch; Git 2.43's missing-promisor probe failure uses a local-object scan.
Base blobs are materialized into a disposable cache, with Git LFS expansion and
payload verification. Depot checkout materialization hydrates its own tracked files.
Whole-blob memory use and very large repository indexing remain limitations. No automatic cache GC
is implemented. The component commits and LFS objects must remain available from
their origins for reconstruction on another machine.

One shared user daemon owns the FUSE session and local control socket. The socket
checks peer UID and lives in a private state directory. Systemd units are generated
for explicit startup, never enabled automatically. Stopping the daemon affects
all workspaces; an edit lease affects only its own workspace. No remote publishing
or implicit background synchronization occurs.
