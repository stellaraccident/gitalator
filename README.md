# Gitalator (`gtl`)

A Linux FUSE workspace over independent Git repositories. Edit one directory tree;
keep ownership, revisions, exports, and reviews separate for each repository.

This is an initial working MVP. It has persistent SQLite metadata, immutable
content storage, numbered snapshots, explicit layout activation, conflict
resolution, and ordinary Git export/import. It is **not yet a production POSIX
filesystem**; read [limitations and backlog](BACKLOG.md) before moving real work in.

## Build and test

Requires Rust/Cargo, a C toolchain (bundled SQLite), Git, Git LFS, Linux `/dev/fuse`,
and `fusermount3` on PATH. No libfuse development package is required: `fuser` uses
its native Linux mount implementation. GitHub publishing additionally requires
`gh` and your existing Git/SSH and GitHub authentication.

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo test --test fuse -- --ignored
cargo test --test daemon -- --ignored    # actual mounts; fails if FUSE unavailable
scripts/demo.sh                       # local repos, mount, edit, snapshot, export
```

All fixtures and demo artifacts stay in ignored `.tmp/`; Cargo uses `target/`.
Tests never contact remote services. Run the mounted suite explicitly: default
`cargo test` deliberately skips it. No system service or global Git configuration
is installed.

## First workspace

Gitalator defaults to `~/.gitalator`, with shared backing repositories and one
explicitly started user daemon. Run this once (it is safe to repeat in `.bashrc`):

```sh
gtl daemon start
```

This installs/starts a systemd **user** service with an absolute executable and
state path. It does not enable login startup or edit shell files. If no user
manager is available, choose `gtl daemon start --detached` explicitly. For a
foreground process, use `gtl daemon run`. Ordinary commands never start a daemon.

Create a super-repository workspace, including its immediate submodules:

```sh
gtl new-workspace dev --from https://github.com/ROCm/TheRock.git --name therock
cd "$(gtl dir top --workspace dev)"
# Optional: put a convenient symlink outside ~/.gitalator.
ln -s "$(gtl dir top --workspace dev)" ~/therock-dev
```

Creation prints the workspace path; fetch progress goes to stderr. It resolves the
super-repository's default branch (override with `--revision`) and exact committed
submodule pins. `--no-submodules` creates just the super-repository mount. There
is no checked-out intermediate clone. Matching canonical origins share one bare
repository across workspaces; SSH and HTTPS identities remain distinct.

```sh
gtl new-workspace experiment --like dev   # same active pins, clean edits/draft
gtl new-workspace replica --clone dev    # same pins, edits and staged draft
gtl submodules expand --workspace dev    # discover one further level
gtl submodules expand --workspace dev --recursive
cd "$(gtl dir parent)"                    # bounded at the current view root
```

Expansion reads committed metadata at existing pins; it does not refresh branches.
It refuses pending map changes and conflicting edits. `--like` and `--clone` create
independent workspaces without copying snapshot history. `--clone` refuses a source
with an activation in progress.

Mounted layout:

```text
~/.gitalator/mount/
  workspaces/dev/therock/              writable sources, including nested mounts
  readonly/dev/therock/                active base without edits
  snapshots/dev/1/therock/             immutable checkpoint
  control/workspaces/dev/             VERSION_MAP, ACTIVE_VERSION_MAP, STATUS
  control/snapshots/dev/1/             maps and readonly checkpoint base
  control/transactions/<id>/          conflict information and candidate source/
```

Source trees contain no generated control files or implicit `depot` directory.
Edit with ordinary tools; unchanged contents fetch on demand. Snapshots retain
local edits independently of evictable fetched payloads.

```sh
gtl status dev
gtl diff dev
gtl snapshot create dev
gtl daemon status
gtl daemon reload                     # validate/apply ~/.gitalator/config.toml
gtl daemon stop                       # reports busy mounts instead of detaching
# gtl daemon stop --lazy               # explicit detach, including busy cwd
```

Use `--state-dir /absolute/isolated/state` on every command for isolated setups.
`gtl --offline ...` is explicit administration with the daemon stopped. The default
mount is `STATE/mount`; `gtl mount /elsewhere` remains a foreground expert option.
State must live on a local filesystem supporting SQLite locks and fsync.

**This version requires fresh schema-3 state.** Older stores are rejected without
migration. Stop the old binary before replacing it; keep/export any edits you need,
then use a fresh state directory. Starting the new daemon does not delete old state.

Snapshots also run automatically while mounted (2 seconds idle, 30 seconds maximum
between dirty checkpoints). Each successful mutation is already persisted;
checkpointing gives groups of mutations convenient numbered names. All history is
currently retained. Restore creates recovery checkpoints; it does not erase history.

See [commands and workflows](docs/workflows.md), [architecture and storage](docs/architecture.md),
[operations and testing](docs/operations.md), and [the backlog](BACKLOG.md).
