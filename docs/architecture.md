# Architecture and storage

## Data model

A resolved map assigns directory cones to `(repository, commit, subtree)` tuples.
An overlay maps `(mount ID, relative path)` to an immutable entry or tombstone.
A workspace head references a revision; revisions hold map JSON and persistent
overlay deltas linked to their parent. Base changes/restore flatten that chain into
a new root. A snapshot references a head plus the draft map, without copying files.

The visible tree is the deepest-owner base union plus that workspace's overlay.
READONLY uses the same resolved map and an empty overlay. Snapshots use their
captured map/overlay, independent of later refs or draft changes. Transaction
candidates have their own map and overlay until publication.

A file entry contains kind, mode, content reference, and length. Backing references
are `git:<repo>:<oid>`; locally written objects use SHA-256 identifiers. Git objects
are immutable; local writes publish new bytes before their metadata transaction.
There are no writable aliases into immutable storage.

## Persistent state

```text
STATE/
  state.sqlite3[-wal,-shm]   versioned schema, WAL + synchronous FULL
  content/objects/<sha256>  durable user changes; never evicted by cache GC
  fetched/objects/<sha256>  verified materialized backend payload cache
  repos/<id>/           bare Git object databases, promisor fetch configuration
  layouts/                 local Git history of resolved maps
  locks/                   process-shared resource locks
  operations.jsonl         append-only structured operation events
  control.sock             mode-0600 daemon control channel
```

The state root is mode 0700. SQLite stores repository registry, revisions,
workspaces, snapshots, activation candidates/conflicts, exports/review receipts,
Git object-to-payload mappings, immutable tree metadata, stable stat fingerprints,
and settings. Schema `user_version=1` is the initial format; newer unsupported
versions fail. Do not edit the database while a daemon is running.

Payload publication writes a unique temporary file, fsyncs it, renames it, then
fsyncs the parent directory. SHA-256 is checked on reads and publication. Only then
may a SQLite transaction refer to it. Interruption can leak an unreferenced object,
but cannot publish metadata pointing to an uncompleted payload write. Automatic
orphan removal and history compaction are deferred.

Fetched materializations are separate from durable user data even when hashes
match. The payload cache records last access and is evicted toward a byte budget.
Git packfiles, tree metadata, local edits, and retained snapshots are **not** covered
by this budget. Evicted Git payloads can be reconstructed locally or fetched again;
offline availability is limited to retained backend objects and local payloads.

## Backend operations and concurrency

Installed Git handles transports, authentication, packfiles, and promisor objects;
there is no custom Git wire protocol. Fetch requests `blob:none`; servers may fall
back to full packs. Tree enumeration caches recursive metadata without hydrating
blobs. File hydration uses `cat-file`, recognizes LFS pointers, obtains payloads
through Git LFS, and verifies advertised SHA-256 and size.

Independent repositories resolve in bounded groups of four concurrent fetches.
The control server allows up to 16 concurrent requests. Resource locks are `flock`
locks shared across daemon/CLI processes, released automatically on process exit:

| Resource | Scope and reason |
|---|---|
| daemon | One mounted daemon per state directory |
| repository | Fetch and Git object hydration serialize mutations to one bare repo |
| blob/tree key | Coalesce duplicate hydration and metadata population |
| workspace | Serialize write publication, snapshots, freeze and head changes |
| transaction | Serialize candidate edits, resolve and final publication |
| layout | Serialize the local Git index/history writer |
| review export | Serialize publication/reconciliation for one exported checkpoint |
| GC/schema | Serialize cache eviction and schema initialization |

No network fetch runs inside a SQLite write transaction. SQLite briefly serializes
metadata commits; readers use WAL. Activation fetches refs before taking the
workspace lock, then checks that the staged proposal still matches. Candidate
preparation happens after durable freezing and outside the workspace lock.
Final map+overlay publication and unfreezing are one database transaction.

The FUSE adapter currently processes callbacks serially: a cold read can delay
other filesystem calls even though control-plane backend operations run in
parallel. This is a known MVP performance limit, not a global backend lock.

## Activation recovery

Transactions progress PREPARING → READY/CONFLICTED → COMMITTED, or FAILED/ABORTED.
The source head, proposed map, candidate overlay, and conflicts are durable.
A conflicted transaction is editable through its candidate depot. Completion
checks the source head and unresolved conflicts before atomic publication.
Aborting releases the workspace and retains transaction/recovery history.

Crash tests kill subprocesses at payload publication, revision/snapshot commit,
and both sides of activation commit. After a crash, the database exposes the old
or new head, never a partially published map/overlay pair. Secondary layout Git
history and the post-activation checkpoint happen after the core commit and can
lag on interruption; the database remains authoritative.

## Filesystem and build identity

The FUSE adapter exposes ordinary read/write/create/unlink/rename/mkdir/rmdir,
symlinks, chmod/truncate, controls, and immutable snapshots. Mount boundaries are
protected. Files open with direct I/O to avoid stale page-cache contents across
activation. An old open read descriptor retains its base generation; writing an
old descriptor after activation fails and requires reopening.

`user.ws.content_id` returns an opaque stable backing/content identifier.
`user.ws.digest.sha256` returns 32 binary bytes of SHA-256 over visible file bytes
(including an expanded LFS payload). The latter currently reads/hashes the payload.
These are explicitly requested xattrs; listxattr discovery is not implemented.
Stat timestamps use persistent content/mode/size fingerprints so unchanged files
do not receive wall-clock mtimes on each lookup. There is no transparent adoption
of xattr identities by existing build tools; tools must opt in to using them.

Ordinary stat currently hydrates files to obtain the visible size, so LFS sizes
are correct. Very large payloads are processed as whole buffers. Incremental
range hydration, richer kernel caching, POSIX edge cases and build compatibility
are listed in BACKLOG.md.

## Source layout

| File | Responsibility |
|---|---|
| `src/model.rs` | Maps, mount ownership, entries and path validation |
| `src/cache.rs` | Durable immutable objects, atomic writes, locks, structured logs |
| `src/db.rs` | SQLite schema, revisions, workspace heads, snapshots and settings |
| `src/backend.rs` | Bounded Git/LFS processes, fetch, trees and hydration |
| `src/ops.rs` | Composition, mutations, activation, merge, export/import, GC |
| `src/vfs.rs` | FUSE namespace, operations, open handles and checkpoint worker |
| `src/cli.rs` | Commands, control socket, map adapters and import scanning |
| `src/workflow.rs` | Opt-in GitHub publication, verification and bundles |
| `tests/` | Synthetic repositories, persistence, recovery and mounted checks |
