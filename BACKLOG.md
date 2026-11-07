# MVP backlog and known limits

This draft implements the core mechanism and local workflows. Items below are
unfinished or deliberately conservative, not claims of completed functionality.

## Before using this as a primary development filesystem

- **POSIX compatibility:** Direct-I/O files currently do not support mmap, including
  ordinary private mappings used by some compilers/loaders. Build outputs and tools
  that require mappings should live on a normal filesystem. Add a compatibility
  suite covering real compilers, editors, Ninja, linkers and repository tooling.
- **Open inode semantics:** Expand coverage for destination-overwrite descriptors,
  rename/type edge cases and descriptor lifetimes under concurrent mutations. Basic
  rename/unlink identity, multiple unlinked descriptors, directory streams, and
  same-map restore generation fencing now have mounted regression coverage.
- **Metadata:** Directory mtimes track child changes. atime/utimens,
  chown, hardlinks, advisory locks, ACLs, general xattrs, non-UTF8 names, sparse
  files and special files are not implemented. Only the two documented content
  xattrs are exposed; listxattr discovery is pending. Control-file temporary saves
  are in memory until renamed; partial invalid UTF-8 draft writes are unsupported.
- **Git checkout fidelity:** The virtual tree exposes Git bytes with LFS expansion.
  General clean/smudge filters, ident, CRLF normalization, attributes/encoding and
  platform-specific filename policies are not implemented. Add explicit capability
  validation/rejection before using repositories requiring these transformations.
- **Resource limits:** Whole blobs/overlays are buffered in memory; add payload,
  request and workspace quotas, streaming/range I/O and allocation-failure tests.
  Control requests have a size bound, but file writes can still request large buffers.

## Performance and storage

- Extend bounded backend dispatch to remaining blocking mutation/control paths.
  Measure lock contention and expose queue/wait timings and cancellation. Cold
  metadata and file reads now use bounded workers; warm metadata bypasses them.
- Avoid whole-tree metadata enumeration when building the initial persistent index.
  Indexed lookups, shared revision state, checkpoints, and bounded directory caches
  now serve subsequent requests; continue measuring large-overlay publication cost.
- Reduce cold-stat blob transfer requirements and whole-file digest hashing. Exact
  size facts, coherent kernel metadata caching, and bounded recursive fetch lookahead
  are implemented. Read-only mmap remains unsupported.
- Add configurable snapshot expiration, safe history-aware durable object GC,
  orphan cleanup, Git-pack quotas and maintenance. Pin metadata currently has no
  effect because all history is retained. Cache quota covers fetched payloads only.
- Add lock-wait metrics, structured backend error codes and configurable log
  retention. Default activity summaries, detailed transfer logs and rotation exist.
- Richer fetch negotiation/fallback telemetry, retries with jitter, cancellation,
  and broader auth/proxy/SSH overrides. Activation now fetches selected refs or
  missing exact pins with blob filtering and streams per-repository progress. Startup does not automatically resume PREPARING transactions.

## Workflow completion

- More extensive rename/type/mode/deletion conflict tests, duplicate imported paths,
  parent deletion versus new-child conflicts, and candidate partial-merge choices.
- Reconcile the secondary layout Git mirror automatically after a post-commit
  crash. Atomically fork source head/draft and record the new workspace. Validate
  mutual exclusions across CLI choices and improve human status/diff output.
- Export retries currently require a new destination. Add durable resumable export
  jobs, combining multiple mounts of one repo, authorship controls, and explicit
  empty-directory handling. Bundles require all referenced LFS objects locally;
  add automated portable bundle restore/import.
- GitHub review/land adapters are implemented but **not validated on real remotes**.
  Add opt-in disposable remote integration tests for publish/reconcile, auth errors,
  partial completion, LFS, changed PR heads and merge strategies.
- Automatic landing, integration-test attestations, policy hooks, exact reviewed
  head checks, cross-silo dependency ordering, and landed-pin promotion remain
  deferred. Current land verification accepts only exact exported-tree equality.
- Super-repo import reads committed gitlinks and `.gitmodules` only. Add recursive
  imports, staged/index modes, URL policy checks, branch history sharing, and
  detailed nested-cone shadow reports.

## Hardening and delivery

- Crash/fault tests for disk-full, interrupted fetches, SQLite I/O failures, daemon
  reconnect, simultaneous mount/CLI races, and candidate checkpoint boundaries.
- Extend migration and recovery coverage beyond the transactional v1-to-v2 migration
  and its automatic pre-migration backup. Add consistent online backups, store repair
  diagnostics and an explicitly supported Rust/Linux/Git version matrix.
- CI runner provisioning with /dev/fuse, fuzzing of maps/paths/protocol, benchmarks,
  dependency auditing, package/install scripts, and service lifecycle support.
- Store ACLs rely on one OS user and upstream Git authorization. No multi-user
  broker, per-mount credential sandbox or revocation-driven cache purging exists.
