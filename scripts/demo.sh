#!/usr/bin/env bash
# Entirely local, repeatable smoke demo. Leaves artifacts for inspection in .tmp/.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --quiet
mkdir -p .tmp
demo=$(mktemp -d "$PWD/.tmp/demo.XXXXXX")
gtl="$PWD/target/debug/gtl"
state="$demo/state"
mountpoint="$demo/mount"
mkdir "$demo/origin" "$mountpoint"
git -C "$demo/origin" init -q -b main
git -C "$demo/origin" config user.name 'Gitalator Demo'
git -C "$demo/origin" config user.email demo@localhost
printf 'hello from Git\n' > "$demo/origin/hello.txt"
git -C "$demo/origin" add hello.txt
git -C "$demo/origin" commit -qm initial
run() { "$gtl" --state-dir "$state" "$@"; }
run init
run repo add local "$demo/origin"
cat > "$demo/map.toml" <<'MAP'
format = 1
[[mount]]
id = "source"
repo = "local"
path = ""
revision = "main"
MAP
run workspace create dev --map "$demo/map.toml"
run activate dev
run mount "$mountpoint" > "$demo/daemon.log" 2>&1 &
daemon=$!
cleanup() {
    fusermount3 -u "$mountpoint" 2>/dev/null || true
    wait "$daemon" || true
}
trap cleanup EXIT
for ((i=0; i<100; i++)); do
    [[ -f "$mountpoint/workspaces/dev/depot/hello.txt" ]] && break
    kill -0 "$daemon" 2>/dev/null || { cat "$demo/daemon.log"; exit 1; }
    sleep 0.05
done
base="$mountpoint/workspaces/dev/READONLY/hello.txt"
file="$mountpoint/workspaces/dev/depot/hello.txt"
[[ $(cat "$file") == 'hello from Git' ]]
printf 'hello from the workspace\n' > "$file"
[[ $(cat "$base") == 'hello from Git' ]]
run snapshot create dev
run export dev "$demo/export"
[[ $(cat "$demo/export/source/hello.txt") == 'hello from the workspace' ]]
printf 'edited in exported Git worktree\n' > "$demo/export/source/hello.txt"
run import dev "$demo/export"
[[ $(cat "$file") == 'edited in exported Git worktree' ]]
run snapshot list dev
printf '\nDemo passed; artifacts: %s\n' "$demo"
