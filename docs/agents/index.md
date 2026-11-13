# Gitalator guide for agents

Gitalator composes pinned Git repositories and a writable depot overlay into a
FUSE workspace. The physical depot checkout stores the manifest and sparse changes;
it is not the composed source tree.

Read the chapter for the task you are about to perform:

- `gtl agents mount-add`: add mounts and choose submodule import depth.
- `gtl agents workspace-create`: initialize an empty workspace or create one
  from an existing depot.

These guides are embedded in this executable. Reading them requires no server,
network connection, or initialized state. Run `gtl agents --help` for chapters.
Use `gtl --help` for the command reference.

Keep the same `--state-dir` across commands for a particular server. Do not
start a server implicitly, mutate the backing checkout outside an edit session,
or treat `--offline` as a promise of no network activity.
