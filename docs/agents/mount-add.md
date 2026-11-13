# Task: add a repository mount

Run from anywhere inside the composed workspace. Gitalator discovers the workspace
and state through its absolute `.depotgit` link. An explicit `--state-dir` must
match that workspace. The destination is always workspace-relative, not relative
to your current subdirectory; `--at .` means the workspace root.

For the empty `example` workspace:

```sh
cd /path/to/ws/example
gtl mount add https://github.com/ROCm/TheRock.git --at . --submodules recursive
```

Choose exactly how far to import submodules:

- `--submodules none` (default): only this repository. Gitlinks are empty directories.
- `--submodules first-level`: direct submodules become mounts; their nested gitlinks remain empty directories.
- `--submodules recursive`: all recursively referenced submodules become mounts.

The root revision defaults to the remote's HEAD. Use `--revision BRANCH_OR_COMMIT`
to select another revision. The command resolves it to a full immutable commit ID;
submodules always use their exact gitlink pins, never their configured branches.
Relative submodule URLs are resolved against the parent repository origin.

Outside the composed tree, select a workspace explicitly:

```sh
gtl --state-dir /path/to/state mount add /absolute/path/to/repo --at components/repo --workspace example --submodules none
```

An existing mount at the destination is an error. This command adds new mounts;
it does not switch pins, replace populated mounted subtrees, or run a merge.
Importing a root repository can coexist with existing nested mounts if their
paths do not collide with newly imported mounts.

Fetches use blob filtering. Metadata and payload hydration remain lazy. Fetching
and validation happen before activation; the old view remains available during
planning. Close open files, directory handles, and mappings in the target workspace
before activation. Your shell can remain at the workspace root.

The operation changes the active layout and the physical checkout's `depot.toml`.
It leaves the Git index and HEAD alone: the layout is immediately usable but is
not yet committed. Existing sparse overlay files remain above the new base.
Incompatible metadata collisions or tombstone provenance changes are rejected.

The result includes `state_dir`. Other commands still use the selected/default
state, so retain your `--state-dir` flag or the shell wrapper from the creation
chapter for the follow-up commands.

Inspect and persist the result:

```sh
gtl status example
cat .mountcommit
git -C .depotgit diff -- depot.toml
gtl depot edit example -- git add depot.toml
gtl checkpoint example -m 'Add TheRock mounts'
```

Checkpoint commits staged changes, including anything already staged before this
operation: inspect `git -C .depotgit diff --cached` first. It needs your Git author
identity. A live workspace clone includes the active uncommitted layout; a clean
fork uses the committed layout.

`gtl activity` reports import progress and `gtl cancel ID` cancels preparation.
Ctrl-C on the importing CLI also cancels it. Cancellation before activation leaves
the active map/manifest unchanged (downloaded objects may remain cached).
Activation is journaled and completes before observing cancellation. After a crash,
restart recovery finishes a published layout update.
