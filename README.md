# Gitalator (`gtl`)

Depot-backed federated workspaces for Linux. A Git depot stores exact component
pins, sparse replacement files under `overlay/`, and deletion markers under
`tombstones/`. Each workspace owns a linked depot worktree and branch; component
repositories are immutable bases shared through managed bare stores.

This repository is being rebuilt around the depot foundation. Old SQLite stores,
numbered snapshots, activation, and submission commands have been removed.
Existing old state is rejected without modification. Use a fresh state directory.

Build: `cargo build --release --locked`. Test: `cargo test --locked`.
Requires Rust, a C linker, Git, and Linux FUSE for mounted tests. No SQLite or
libfuse development package is required. The workspace campaign specification is
`../../docs/depot-design.md` when checked out in gitalator-ws.
