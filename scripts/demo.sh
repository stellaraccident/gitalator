#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --quiet
mkdir -p .tmp
demo=$(mktemp -d "$PWD/.tmp/depot-demo.XXXXXX")
gtl="${CARGO_TARGET_DIR:-$PWD/target}/debug/gtl"
for repo in component depot; do
    git init -q -b main "$demo/$repo"
    git -C "$demo/$repo" config user.name 'Gitalator Demo'
    git -C "$demo/$repo" config user.email demo@localhost
done
printf 'hello\n' > "$demo/component/hello"
git -C "$demo/component" add .
git -C "$demo/component" commit -qm initial
pin=$(git -C "$demo/component" rev-parse HEAD)
cat > "$demo/depot/depot.toml" <<MAP
format = 1
[repositories.code]
url = "$demo/component"
[[mounts]]
id = "code"
repository = "code"
commit = "$pin"
path = "src"
MAP
git -C "$demo/depot" add .
git -C "$demo/depot" commit -qm depot
run() { "$gtl" --state-dir "$demo/state" "$@"; }
run --offline workspace create dev --from "$demo/depot" --revision main
run daemon start --detached
cleanup() { run daemon stop --lazy >/dev/null 2>&1 || true; }
trap cleanup EXIT
printf 'edited\n' > "$demo/state/mount/dev/src/hello"
printf 'new\n' > "$demo/state/mount/dev/new"
run depot edit dev -- git config user.name 'Gitalator Demo'
run depot edit dev -- git config user.email demo@localhost
run depot edit dev -- git add overlay
run checkpoint dev -m 'Sparse depot change'
run workspace clone experiment --from dev
cmp "$demo/state/mount/dev/src/hello" "$demo/state/mount/experiment/src/hello"
run diff dev
printf 'Demo passed; artifacts: %s\n' "$demo"
