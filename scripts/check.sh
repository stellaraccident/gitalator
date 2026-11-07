#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ $# -gt 1 || ( $# -eq 1 && $1 != --fuse ) ]]; then
    printf 'usage: %s [--fuse]\n' "$0" >&2
    exit 2
fi
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
if [[ ${1:-} == --fuse ]]; then
    cargo test --locked --test fuse -- --ignored
    scripts/demo.sh
fi
