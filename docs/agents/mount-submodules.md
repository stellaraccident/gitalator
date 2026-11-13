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
