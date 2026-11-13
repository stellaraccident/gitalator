# Task: remove repository mounts

From inside a composed workspace, select an exact workspace-relative mount path:

```sh
gtl mount remove --at .
gtl mount remove --at components/library --recursive
```

The first command removes only the root mount, retaining every descendant mount.
`--recursive` removes the selected mount and its entire cone of descendant mounts,
including independently added mounts. It does not remove siblings with similar
prefixes. A missing mount is an error. Paths are always workspace-relative.
Outside the workspace, supply `--workspace NAME` and the correct `--state-dir`.

Depot overlay files are preserved, as are staged changes, Git HEAD, backing bare
repositories, and downloaded objects. Preserved files may become freestanding
files or overlay the newly exposed parent base. A removed submodule mount leaves
its parent gitlink as an empty directory unless retained descendants or overlay
files populate it. Unused repository declarations are removed from the manifest.

Tombstones carry mount and base provenance. If removal would invalidate a
tombstone, the whole operation fails before activation. There is no force flag
that discards deletions. Inspect `.depotgit/tombstones` and resolve the affected
intent first. If explicitly abandoning a deletion, remove only its marker in a
`gtl depot edit NAME -- ...` session, then retry; that reveals the base entry.
Other overlay/namespace conflicts also reject the operation without activation.

Close open workspace files, directory handles, and mappings before activation.
The shell can remain at the workspace root. Preparation is cancellable via Ctrl-C
or `gtl cancel ID`; activation uses the recoverable layout journal.

The active layout and physical `depot.toml` change immediately, but are not staged
or committed. Inspect and checkpoint using the same state selection as your server:

```sh
git -C .depotgit diff -- depot.toml
git -C .depotgit diff --cached
gtl depot edit example -- git add depot.toml
gtl checkpoint example -m 'Remove repository mounts'
```

Checkpoint includes all staged changes. Removing the root mount does not remove
the workspace or its `.depotgit` discovery link.

## Lazy discovery and validation

Startup and layout activation prepare mount roots and validate boundaries and
edited-path/tombstone dependencies. They do not scan every component directory.
Untouched directory errors surface when accessed, with mount/path diagnostics in
activity logs. Use `gtl validate NAME --full` for a cancellable whole-base check;
normal `gtl validate NAME` is targeted. Full validation warms directory caches
without hydrating payloads. `gtl status NAME` reports shared `tree_cache` counters.
Explicit submodule discovery still explores the requested submodule graph.
