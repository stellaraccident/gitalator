# Session handoff — 2026-11-07

## Status

Standalone Rust repository, binary `gtl`. User-wide ergonomics now use schema 3,
shared canonical-origin bare repositories, explicit daemon startup, one-command
super-repository workspaces, and parallel source/control namespaces. Older state
must be exported with its matching binary and replaced with fresh state; no
migration is provided. See Git history for validated implementation milestones.
No remote publication or user login configuration changes were performed.

Read README.md, docs/architecture.md, docs/workflows.md, docs/operations.md and
BACKLOG.md. These contain the implementation contract, commands, failure recovery,
and unfinished work; no prior conversation is required to continue development.
All implementation artifacts are inside this repository. Earlier research in the
old machine's `~/docs` is supplementary and is not required to build or run it.

## Move the repository

This is an independent Git repository nested in an unrelated firmware workspace.
It has no dependency on that parent repository, its submodules, Conan, or toolchains.
Move/clone **gitalator itself**, including its Git history. Do not copy the parent.

A Git bundle is a convenient transport that includes all committed refs/history:

```sh
# On the old machine, from inside gitalator/:
git status --short --ignore-submodules
git bundle create /path/to/transfer/gitalator.bundle --all
git bundle verify /path/to/transfer/gitalator.bundle
# Transfer the bundle, then on the new machine:
git clone -b main /path/to/transfer/gitalator.bundle gitalator
cd gitalator
# The clone's origin points at the transport bundle. Set your real origin later:
# git remote set-url origin <real-repository-URL>
```

Alternatively copy the directory including `.git/`, excluding `target/`, `.tmp/`
and any disposable `.gtl/`. Do not omit `.git/` if preserving history matters.
`target/` is ~1.3 GiB of rebuildable output; `.tmp/` contains disposable synthetic
fixtures and demo/log artifacts. The bundle excludes both automatically. No
uncommitted source files need separate transport at handoff.

**Real workspace data is different from source code.** If you subsequently put
valuable edits into a Gitalator state directory, it is ignored by Git and will not
be included in a source bundle. Stop/unmount its daemon and copy the entire state,
including SQLite WAL files, content and Git stores, as described in operations.md.
State is not fully relocatable yet: registered local repo origins, export manifest
paths, and export/review database records can contain absolute machine-local paths.
For this session only synthetic disposable workspaces were created. Start a fresh
state on the new machine. Git bundles of source repos do not include LFS payloads.

## New-host prerequisites and verification

Linux with working `/dev/fuse` and `fusermount3`; Rust/Cargo, C build toolchain,
Git and Git LFS. SQLite is bundled; no libfuse headers/pkg-config are needed for
the selected native `fuser` configuration. A container must expose `/dev/fuse`
and permit FUSE mounts. `gh` and noninteractive Git credentials are needed only
for opt-in remote review operations. Existing SSH credentials are not part of
this repo and should be provisioned separately.

```sh
cargo build --locked
target/debug/gtl --offline --state-dir .tmp/doctor-state doctor
scripts/check.sh --fuse
cargo build --release --locked
```

Initial host validation passed: formatting; Clippy with warnings denied; 23 default
tests; two explicitly enabled real FUSE tests; local mount/edit/snapshot/export/import
demo; release build. Rust 1.99.1, Git 2.43, Git LFS 3.4 were installed. Dependencies
are locked in Cargo.lock; the supported minimum Rust version is not established.
The default suite uses only synthetic local repositories. Crash-injection tests
need the debug profile. No actual remote publication/landing was tested.

## Decisions to preserve

- Parallel readonly, writable workspace and control trees, with deepest directory-cone ownership.
  No union of competing files, and no editable hardlinks into backend storage.
- Editable VERSION_MAP is a proposal. Activation is explicit and uses durable
  freeze/candidate/conflict/publication states. Draft edits remain independent.
- Numbered snapshots preserve complete workspace maps/overlays/drafts; retained
  history is not evicted by fetched-payload cache GC.
- Git is a per-silo backend; export ordinary Git worktrees/commits for review.
  Cross-repository landing cannot be atomic and must respect each silo's policy.
- Persistent SQLite plus immutable payloads; bytes are durable before database
  references. Serialize shared resources, allow independent repos to progress.
- Never silently publish remotely. `review` plans; `review --publish` opts in.

## Next work

Consult BACKLOG.md for the full list. Prioritize filesystem compatibility and
correctness before large real workspaces: mmap, inode/open-descriptor semantics,
directory mtimes and checkout transformations. Then address serial FUSE callbacks,
whole-file buffering and tree/revision-chain scaling. Add disposable, opt-in real
remote tests before relying on GitHub adapters. Automatic policy-driven landing,
CI attestations and landed-pin promotion are not implemented.

Keep local test artifacts ignored and in `.tmp/`. Use meaningful local commits;
no permission to publish to a new remote is implied by this handoff.
