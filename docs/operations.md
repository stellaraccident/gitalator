# Operations and development

## Running and observing

`gtl doctor` reports prerequisites. `gtl mount PATH` stays in the foreground;
redirect stdout/stderr to a private log when backgrounding it. The daemon owns
one state directory, serves CLI commands on a private Unix socket, and runs
periodic snapshot/cache maintenance. CLI commands work directly against the same
storage while no daemon is present. Interprocess locks remain authoritative.

`operations.jsonl` records timestamps, operation IDs, PID, operation, phase/status, elapsed
milliseconds, and optional detail. Fetches identify repository aliases; hydration
identifies object IDs. Commands, fetches and hydration emit explicit succeeded/failed outcomes. Failures
produce a failed event followed by the span's
finished event; a finished event alone is not a success receipt. CLI stderr carries
the actionable backend error. URLs are omitted from structured events; common
URL/credential forms are redacted from Git errors. This is not a comprehensive
secret scrubber: use credential-free URLs and protect logs. Logs do not rotate yet.

| Setting | Default | Control |
|---|---:|---|
| Snapshot inactivity quantum | 2 seconds | `config --idle-seconds N` |
| Maximum dirty checkpoint interval | 30 seconds | `config --max-seconds N` |
| Fetched-payload cache target | 1 GiB | `config --cache-bytes N` |
| Backend Git deadline | 300 seconds | `GTL_BACKEND_TIMEOUT_SECS` |
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

1. Stop competing writers. After a daemon crash, unmount with `fusermount3 -u PATH`
   (use `-uz` only to detach an already-dead mount).
2. Restart `gtl --state-dir STATE mount PATH`. Startup acquires the daemon lock and
   replaces a stale control socket. Merely finding a socket does not prove a live
   daemon; offline commands deliberately fail rather than bypass it.
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
The format has no automated migrations beyond its initial schema version yet.

## Testing discipline

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
cargo test --test fuse -- --ignored
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

## Initial validation

Validated locally on 2026-11-07: formatting, Clippy with warnings denied, 23 default
tests, two explicitly enabled mounted FUSE tests, the documented local demo, and
an optimized release build. Tools on this host: Rust 1.99.1, Git 2.43, Git LFS 3.4.
No real-remote GitHub publication or landing was exercised.
