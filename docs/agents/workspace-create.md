# Task: create a workspace

## Choose the flow

- New empty workspace and local depot: `gtl workspace init NAME`.
- Workspace backed by an existing depot: `gtl workspace create NAME --from REMOTE`.
- Existing workspace's committed state or live edits: use `workspace fork` or
  `workspace clone`; do not initialize another depot for these.

Use `example` as the name for this walkthrough. Workspace names contain ASCII
letters, digits, hyphens, or underscores, and cannot start with a dot. An existing
name is an error: initialization never replaces a workspace.

## 1. Select the state and check the server

For the default state at `~/.gitalator_state`, run:

```sh
gtl daemon status
```

For an isolated scratch evaluation, use the same absolute state path in every
terminal and on every command. A shell function can keep this consistent:

```sh
export STATE=/absolute/path/to/workspace/scratch/journey/state
gtl() { /absolute/path/to/release/gtl --state-dir "$STATE" "$@"; }
gtl daemon status
```

If the server is already running, reuse it. If it is stopped and you want the
composed FUSE view, explicitly run this in a dedicated terminal:

```sh
gtl daemon run
```

This stays in the foreground and prints progress when attached to a terminal.
It installs no service. Keep that terminal running; use a second terminal for
the commands below. Both terminals must share the mount namespace.

The default shared mount is `~/ws`; an explicitly selected state directory
defaults to `STATE/mount`. To customize it, run
`gtl --offline config --mount /absolute/path/to/ws` before starting the server.

## 2. Initialize the empty workspace

```sh
gtl workspace init example
```

This creates a new managed bare depot, an initial commit containing an empty
`depot.toml`, and a linked worktree on an independent workspace branch.
There is no origin remote, no component mount, and no network fetch. Only the
bootstrap commit uses Gitalator's author identity; your Git identity is not
overwritten.

The JSON result reports:

- `path`: the composed workspace path; use this as your source directory.
- `depot_worktree`: the physical sparse checkout.
- `depot_repository`: its managed bare Git repository.
- `branch`: the workspace's depot branch.

Use the returned `path` rather than assuming a mount location.

## 3. Verify the result

```sh
gtl workspace list
gtl status example
gtl validate example
gtl diff example
gtl dir example
```

Expect `example` in the list, validation success, and an empty diff (`[]`).
At the returned workspace path, `ls -la` shows only the generated discovery
entries `.depotgit` and `.depotstate.json`, apart from `.` and `..`.
There is no `.mountgit` because this workspace has no component mounts yet.
The `.depotgit` symlink points to the checkout, not its Git administrative directory.

Read-only backing Git inspection is safe:

```sh
git -C /returned/workspace/path/.depotgit status --short
cat /returned/workspace/path/.depotgit/depot.toml
```

Git status should be clean. The manifest has no repositories or mounts.
Do not run `git init` in the composed view. Future source files belong in that
view; writes are stored under the depot checkout's `overlay/`. Backing Git
mutations use `gtl depot edit`, as subsequent task chapters will describe.

## Existing depot alternative

```sh
gtl workspace create example --from /absolute/path/to/depot --revision main
```

A URL also works as the remote. The depot must already contain a committed,
valid `depot.toml`. Gitalator fetches that depot and the component pins in its
manifest. This is different from initializing a new, empty depot.

## Without a running server

You can initialize and inspect the persistent state directly:

```sh
gtl --offline workspace init example
gtl --offline validate example
```

Then run `gtl daemon run` when ready to browse the FUSE view. The returned source
path is not a usable composed filesystem until mounted. Never use `--offline`
while a server owns this state. The flag bypasses the server; other offline
commands can still fetch from the network.

## Errors and activity

Use `gtl activity` to see work in progress and `gtl cancel ID` to cancel it.
Persistent activity logs are under `STATE/logs/activity.jsonl`.
If the server cannot be reached, check `daemon status` and confirm the state
path. Initialization does not start it for you.

After an interrupted initialization, inspect `workspace list` and `status`
before retrying. Restart recovery finishes a registered initialization or leaves
an explicit repair lease. Do not delete backing state as a retry strategy.

## Lazy discovery and validation

Startup and layout activation prepare mount roots and validate boundaries and
edited-path/tombstone dependencies. They do not scan every component directory.
Untouched directory errors surface when accessed, with mount/path diagnostics in
activity logs. Use `gtl validate NAME --full` for a cancellable whole-base check;
normal `gtl validate NAME` is targeted. Full validation warms directory caches
without hydrating payloads. `gtl status NAME` reports shared `tree_cache` counters.
Explicit submodule discovery still explores the requested submodule graph.
