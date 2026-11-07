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
cargo build --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
cargo test --test fuse -- --ignored    # actual mounts; fails if FUSE unavailable
scripts/demo.sh                       # local repos, mount, edit, snapshot, export
```

All fixtures and demo artifacts stay in ignored `.tmp/`; Cargo uses `target/`.
Tests never contact remote services. Run the mounted suite explicitly: default
`cargo test` deliberately skips it. No system service or global Git configuration
is installed.

## First workspace

Use an absolute state path consistently. The mount and state directories must be
separate, and the mount directory must be empty. State contains private source
and edits; use a local filesystem with working SQLite locks and fsync.

```sh
GTL="$PWD/target/debug/gtl"
STATE="$PWD/.gtl"
"$GTL" --state-dir "$STATE" init
"$GTL" --state-dir "$STATE" repo add engine git@github.com:example/engine.git
cat > VERSION_MAP <<'MAP'
format = 1
[[mount]]
id = "engine"
repo = "engine"
path = "src/engine"
revision = "main"
MAP
"$GTL" --state-dir "$STATE" workspace create dev --map VERSION_MAP
"$GTL" --state-dir "$STATE" activate dev
mkdir mount
"$GTL" --state-dir "$STATE" mount "$PWD/mount"  # foreground; second terminal below
```

Mounted layout:

```text
mount/
  workspaces/dev/
    VERSION_MAP          editable proposal, including temporarily invalid TOML
    ACTIVE_VERSION_MAP   resolved, immutable until explicit activation
    STATUS
    READONLY/            active base layout, without workspace modifications
    depot/               writable composed source tree
  snapshots/dev/1/       immutable VERSION_MAP, ACTIVE_VERSION_MAP, READONLY, depot
  snapshots/dev/2/       …
  transactions/<id>/     candidate depot and conflict information
```

Edit `mount/workspaces/dev/depot/...` with ordinary tools. There are no hardlinks
from editable files into Git storage. Unchanged bytes are fetched on demand;
local changes have their own durable, immutable content objects.

```sh
"$GTL" --state-dir "$STATE" status dev
"$GTL" --state-dir "$STATE" diff dev
"$GTL" --state-dir "$STATE" snapshot create dev
"$GTL" --state-dir "$STATE" snapshot list dev
"$GTL" --state-dir "$STATE" restore dev 1
"$GTL" --state-dir "$STATE" unmount "$PWD/mount"
```

Snapshots also run automatically while mounted (2 seconds idle, 30 seconds maximum
between dirty checkpoints). Each successful mutation is already persisted;
checkpointing gives groups of mutations convenient numbered names. All history is
currently retained. Restore creates recovery checkpoints; it does not erase history.

See [commands and workflows](docs/workflows.md), [architecture and storage](docs/architecture.md),
[operations and testing](docs/operations.md), and [the backlog](BACKLOG.md).
