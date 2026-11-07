# Operations and development

## Running and observing

`gtl daemon start` explicitly starts the per-state systemd user service and waits
for the FUSE mount and control endpoint to be ready. Repeated calls return the
existing daemon; normal startup is quiet for use in `.bashrc`. No shell files are
edited and the unit is not enabled automatically. Each root gets a distinct unit
name. Without a user manager, startup fails with an explicit `--detached` suggestion.
Detached mode uses a new session and a stable working directory; foreground
`daemon run` and expert `mount PATH` are also supported. No ordinary command
starts a process or silently falls back to offline mutation. Use `--offline` for
explicit administration; it requires the exclusive daemon ownership lock.

`daemon status` reports readiness, PID, UID, executable, version, startup mode,
state root, mount, protocol and log locations. The private control socket verifies
same-UID peers and protocol versions. Startup is serialized; the ownership lock
remains held through accepted commands and worker drainage. `daemon stop` unmounts
normally and reports a busy mount; `daemon stop --lazy` explicitly detaches one.
The systemd unit uses lazy detach for service shutdown. SIGINT/SIGTERM also detach
and drain. Changing the executable on disk does not upgrade a running daemon.

`STATE` defaults to `~/.gitalator_state`; the default user mount is `~/.gitalator`.
An isolated `--state-dir` retains `STATE/mount` unless configured otherwise.
`STATE/config.toml` is authoritative; missing keys use defaults:

```toml
# mount = "/absolute/alternate/mount"  # restart required; default ~/.gitalator for the user-wide state
snapshot_idle_seconds = 2
snapshot_max_seconds = 30
cache_bytes = 1073741824
metadata_cache_bytes = 536870912
backend_timeout_seconds = 300
```

Edit the file, then `gtl daemon reload`. Unknown fields, invalid limits, and mount
changes are rejected before any settings apply. Cache, snapshot and Git deadline
limits apply live to subsequent work. Existing `gtl config` flags atomically update
this file and derived settings together. Within the state directory, only the
reserved `STATE/mount` location is mountable; alternate mounts must be outside state. The state root cannot be relocated by
reload. `--state-dir` selects an independent root at process startup.

`logs/daemon.log` captures lifecycle, reload, activity summaries and errors.
`logs/operations.jsonl` records timestamps, operation IDs, PID, operation, phase/status, elapsed
milliseconds, and optional detail. Fetches identify repository aliases; hydration
identifies object IDs. Commands, fetches and hydration emit explicit succeeded/failed outcomes. Failures
produce a failed event followed by the span's
finished event; a finished event alone is not a success receipt. CLI stderr carries
the actionable backend error. URLs are omitted from structured events; common
URL/credential forms are redacted from Git errors. This is not a comprehensive
secret scrubber: use credential-free URLs and protect logs. Logs rotate at 8 MiB,
retaining the current file plus three numbered backups (`operations.jsonl.1`–`.3`).
Records are serialized across daemon threads, redacted, and detail fields bounded.

By default the mount prints its log location at startup, immediate backend failures,
and at most one activity summary every five seconds while work is active (plus a
completion summary). Summaries show repository IDs and short origins, active counts,
phases, elapsed time, and completed/failed operations. Warm metadata requests are
silent. Git transfer progress stays in the detailed log instead of flooding the
mount console. LFS payload work is also tracked. To follow the default store:

```sh
tail -F ~/.gitalator_state/logs/daemon.log ~/.gitalator_state/logs/operations.jsonl
```

Missing-blob fetches use Git's no-op negotiation, skip recursive submodule fetching
and automatic maintenance, and share the repository mutation lock with activation
and file hydration. Each requested blob is checked locally after fetching. A failed
fetch is accepted only if every requested blob verifies locally; otherwise recognized
transient transport/connectivity failures get one retry after 250 ms. Authentication
and unavailable-object errors are not blindly retried. Detailed records include the
requested object IDs, retry/failure details and outcomes; `metrics` includes
`fetch_retries` and `fetch_failures`. A failed size lookup is not cached as zero or
as a permanent failure: a later stat can retry. Restart the mount after replacing
its binary; existing cached repositories do not need to be deleted.

| Setting | Default | Control |
|---|---:|---|
| Snapshot inactivity quantum | 2 seconds | `config --idle-seconds N` |
| Maximum dirty checkpoint interval | 30 seconds | `config --max-seconds N` |
| Fetched-payload cache target | 1 GiB | `config --cache-bytes N` |
| Metadata memory budget | 512 MiB | `config --metadata-cache-bytes N` |
| Backend Git deadline | 300 seconds | `backend_timeout_seconds` in config (environment override: `GTL_BACKEND_TIMEOUT_SECS`) |
| Resource-lock wait deadline | 300 seconds | `GTL_LOCK_TIMEOUT_SECS` |
| GitHub CLI deadline | 120 seconds | fixed in initial adapter |

Set environment limits when starting the daemon. These are operation deadlines,
not one deadline for a whole multi-repository activation. LFS smudge has a separate
300-second deadline. Expired subprocesses are killed as a process group so helpers
do not keep pipes open. Credentials must be available noninteractively; SSH uses
`ssh -oBatchMode=yes` and currently overrides `GIT_SSH_COMMAND`. User SSH config
and agent remain available; system Git config is disabled for child operations.

`gc --dry-run --max-bytes N` reports payload eviction. `gc --max-bytes N` performs
it; `config --cache-bytes N` sets the daemon's periodic target. GC never prunes
history. Disk use can exceed that target substantially because Git objects,
durable overlays, metadata and logs are outside it.

## Recovery

1. After a crash, `gtl daemon start` verifies a disconnected gitalator mount at
   the configured mountpoint, detaches it while holding the ownership lock, and
   restarts. It never takes over a live owner or detaches an unidentified mount.
   Expert `mount PATH` reports stale mounts for explicit recovery with
   `fusermount3 -uz PATH`. Leave the old tree before continuing work.
2. Startup replaces a stale socket only under the ownership lock. A protocol
   mismatch requires stopping the old daemon with its matching binary first.
3. Inspect `status WORKSPACE`, transaction STATUS/conflicts, and operation logs.
   READY/CONFLICTED candidates remain available. Resolve and `activate --continue`,
   or `activate --abort`. An interrupted PREPARING/FAILED transaction requires abort
   and retry; automatic preparation replay is not implemented.
4. Inspect snapshots before restoring. Restore itself saves recovery checkpoints.
   Core map/head publication is atomic; after a crash immediately following commit,
   the local layout Git mirror/post-commit snapshot may lag. Calling `layout branch`
   for a new label re-records the active map from authoritative database state.

Do not delete cache/database files to fix an error. Back up the **entire** state
with the daemon stopped, including Git stores and both content areas. A copy of
`state.sqlite3` alone while WAL writers run is not a safe backup. Exported ordinary
Git clones/bundles provide an additional, independently inspectable recovery route.
Schema 3 requires fresh state: old versions fail before store modification. There
is no automatic migration. Export needed work with the matching old binary, then
choose a fresh state directory. Offline commands and mounts require the exclusive
daemon ownership lock. Direct library writers
using an independently opened Store while mounted are unsupported.

## Testing discipline

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo test --test fuse -- --ignored
cargo test --test daemon -- --ignored
scripts/demo.sh
cargo build --release --locked
```

`tests/common` constructs disposable local Git repositories under `.tmp/`.
Storage tests cover integrity/deduplication, concurrent writers, transactional
rollback, reopening and snapshot sharing. Backend tests cover subprocess deadlines,
coalesced hydration, metadata-only enumeration, eviction isolation and local LFS.
Workspace tests cover cone ownership, snapshot/restore, conflict/abort/resolve,
export/import on diverged state, independent layout history and portable bundles.
Recovery tests terminate child processes at selected commit boundaries and reopen
the database. Those failpoints exist only in debug builds; run recovery tests with
the default debug profile. The mounted tests exercise real syscalls, xattrs, control
file saves, old open descriptors, activation and socket communication.

`./scripts/check.sh` runs formatting, lint and the default suite; `--fuse` adds the
mounted checks and demo. Tests use no GitHub credentials or network remotes.
Future real-remote tests must be opt-in, use disposable repositories and explicit
allowlists, and verify failure/retry behavior without publishing into user projects.

When adding a persistent mutation: publish payloads before DB references, hold the
smallest resource lock, keep network work outside SQL transactions, and add an
observable failure/reopen test. Use immutable checkpoint IDs across long-running
operations. Keep tests focused on externally meaningful invariants.

## Validation

Validated locally on 2026-11-07: formatting, Clippy with warnings denied, 56 default
tests, 11 explicitly enabled mounted FUSE/daemon tests, the local demo, and an
optimized release build. The systemd launch test uses a fake manager that executes
the generated unit command; the real user manager was not changed or exercised. Tools on this host: Rust 1.99.1, Git 2.43, Git LFS 3.4.
No real-remote GitHub publication or landing was exercised.

## Directory lookup performance

Directory lookups use persistent immediate-child Git indexes, cached view snapshots,
and copy-on-write revision state. Revision checkpoints limit cold replay; unchanged
directory vectors remain reusable across edits. Warm FUSE lookups/listings perform
no database, Git, payload, or atime writes. Unsupported xattrs return immediately.

`opendir` captures an ordered directory stream. Cookies remain stable until rewind;
rewind captures current state. `readdirplus` supplies exact cached attributes, or
name-only records when sizes are unknown or a stream predates publication. It does
not hydrate files. A stat after a source-directory listing resolves up to 256
unknown sibling blobs. If the parent directory was also listed, the daemon treats
this as a recursive scan: it combines up to 1,024 distinct blobs from at most 32
directories (the current directory plus immediate children and siblings). It uses
the indexed owner tree, stays within the same mount, and does not follow symlinks
or descend further. Later stats reuse those facts; larger scans use successive
batches. Name-only traversal still performs no fetch, and an isolated stat without
listing intent resolves only its requested object.

The bounds count objects and directories, not bytes: sizes are unknown until Git
delivers objects. Lookahead may download files a traversal ultimately skips. If a
speculative sibling fails, the requested file is retried independently and speculation
is disabled for that directory revision. Explicit warmup also uses batches of up to
1,024 distinct blobs. Demand blob fetches pass queued lookahead at the per-repository
fetch gate; an already-running Git transfer cannot be preempted. Other repositories
use independent gates, and cached metadata stays on the immediate FUSE reply path.

Four background workers and a 128-job queue handle cold metadata;
queue saturation returns EAGAIN. File reads also use these workers so a download
does not block unrelated warm metadata requests.

Metadata has a separate estimated-memory budget (default 512 MiB), including pinned
directory streams. It uses eviction for unpinned records and returns ENOMEM if
pinned records prevent a fill. Configure with:

```sh
gtl config --metadata-cache-bytes 536870912
gtl warmup dev                       # root directory only
gtl warmup dev compiler/amd-llvm      # immediate children of this directory
gtl warmup dev compiler --recursive  # explicit recursive metadata warmup
gtl --json metrics
```

Warmup paths are workspace-relative, without `.` or `..`; it does not follow symlinks.
It captures a revision, streams progress to stderr, and returns counts/failures;
any failures produce a nonzero exit status. Exact sizes are persisted separately
from evictable payloads. Missing Git objects are fetched in batches; small blobs
are inspected for LFS pointers, whose logical sizes require no LFS payload download.
Git servers may still transfer complete ordinary blobs to reveal their sizes.

The daemon publishes a shared epoch after authoritative commits. Responses computed
against old epochs are retried. External mutation commands wait for kernel attribute
and dentry invalidation before returning; notification failure marks the mount
unhealthy and attempts unmount. Notifications currently invalidate all observed
aliases conservatively. Positive source metadata TTL is one second; negative replies
and mutable control files are uncached. Files use direct I/O (no mmap/writeback),
and only immutable snapshot directories enable kernel directory caching.

Restart the mount daemon to use a rebuilt binary. Replacing an executable does not
upgrade a running process. Keep a full stopped-daemon state backup before destructive changes.

`python3 scripts/benchmark-listings.py /path/to/gtl /new/scratch/directory` builds a
local fixture and measures 100 iterations in one Python process, without shell or
process-start timing. The destination must not exist; it retains results and logs.
The release benchmark on this host measured warm p95 of 0.16 ms for 100 names,
4.15 ms for 10,000 names, and 0.46 ms for 100 names plus stat. Name-only iterations
performed zero SQL/Git/payload work. Periodic maintenance can increment SQL counters
independently of measured requests; the metadata regression also checks the no-I/O
invariant with the maintenance thread absent.
