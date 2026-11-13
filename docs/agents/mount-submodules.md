# Task: expand submodules in an existing mount

From inside the composed workspace:

```sh
gtl mount submodules --at . --depth first-level
gtl mount submodules --at . --depth recursive
```

`--at` selects an exact workspace-relative mount path; `.` is the root.
The default depth is `first-level`. Outside the workspace, pass `--workspace NAME`
and the matching `--state-dir`.

Expansion reads gitlinks and `.gitmodules` from the mount's current pinned commit,
not from the remote branch tip or locally modified overlay files. Each imported
child uses its exact gitlink pin. Relative URLs resolve against the parent's
repository origin. For subtree mounts, only gitlinks within the subtree are
expanded, using the repository-root `.gitmodules` declarations.

First-level expansion adds direct submodule mounts. Recursive expansion also
expands their descendants. Matching existing mounts are reused with their IDs
intact. Repeating the operation is harmless; first-level expansion never removes
already expanded descendants. Unrelated mounts remain unchanged.

An existing child mount with a different URL, pin, or subtree is a conflict:
the command reports the path and activates nothing. Expansion does not repin,
prune obsolete mounts, merge depot patches, or import locally edited submodule
configuration. Changes in parent revisions require the planned update/resolve
workflow, which is not implemented yet. Explicit removal and re-addition are
separate operations, not a substitute for resolving patches against a new base.

Fetches retain blob filtering; file payloads remain lazy. `gtl activity` reports
work. Ctrl-C or `gtl cancel ID` cancels preparation without changing the active
layout (downloaded objects may remain). Close files, directory handles, and mappings
before activation. Other workspaces remain usable while preparation fetches.

The active layout and physical `depot.toml` update atomically and remain uncommitted.
The result reports `added`, `removed`, `updated`, and `changed`; an unchanged result
does not rewrite the manifest or generation. Inspect and checkpoint:

```sh
git -C .depotgit diff -- depot.toml
git -C .depotgit diff --cached
gtl depot edit example -- git add depot.toml
gtl checkpoint example -m 'Expand pinned submodules'
```

Keep your server's state selection for those commands. Checkpoint includes all
staged changes and needs your Git author identity.

## Inaccessible submodules

Add `--skip-unavailable` to import accessible children even if another child's
fetch fails. Without this flag, a failed fetch aborts the layout operation.

```sh
gtl mount submodules --at . --depth recursive --skip-unavailable
```

The same flag is available on `mount add --submodules first-level|recursive`.
A skipped child remains unexpanded (normally an empty gitlink directory); existing
mounts and overlay files are preserved. Successful parents and siblings are kept.
The result uses `complete: false` and a `skipped` array with each workspace-relative
`path`, sanitized origin, exact `commit`, Git `error`, and shell-quoted
`retry_command`. A partial import exits successfully; scripts must inspect
`complete` to require full population. Errors are also logged as warnings.

Paste a reported `retry_command` into a normal interactive terminal after the
operation finishes to resolve SSH host verification or credentials. Then rerun
the expansion command: matching mounts are reused and missing children are retried.
The retry fetch only populates the shared bare repository; rerunning Gitalator
activates the missing mount. SSH/Git background operations have no controlling
terminal and disable askpass prompts; host-key verification is never bypassed.

Only failed child fetches are skipped. A failed root fetch, cancellation,
conflicting existing pins/origins/subtrees, malformed submodule metadata, or
invalid overlay/tombstone state still aborts activation. Timeouts and local
process/storage failures also remain fatal. Preparation can leave downloaded
objects in the cache even when activation is aborted.

## Lazy discovery and validation

Startup and layout activation prepare mount roots and validate boundaries and
edited-path/tombstone dependencies. They do not scan every component directory.
Untouched directory errors surface when accessed, with mount/path diagnostics in
activity logs. Use `gtl validate NAME --full` for a cancellable whole-base check;
normal `gtl validate NAME` is targeted. Full validation warms directory caches
without hydrating payloads. `gtl status NAME` reports shared `tree_cache` counters.
Explicit submodule discovery still explores the requested submodule graph.
