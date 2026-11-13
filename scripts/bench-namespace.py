#!/usr/bin/env python3
"""Compare eager/lazy release binaries on local many-mount FUSE forests.

Creates only an isolated timestamped trial below --output. Requires /dev/fuse.
Measures already-fetched repositories: initial workspace creation is setup, then
only derived indexes/tree caches are cleared before the cold daemon start.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--before", type=Path, required=True)
parser.add_argument("--after", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--mounts", type=int, default=679)
parser.add_argument("--origins", type=int, default=8)
parser.add_argument("--small-files", type=int, default=64)
parser.add_argument("--large-files", type=int, default=1024)
args = parser.parse_args()
if min(args.mounts, args.origins, args.small_files, args.large_files) < 1:
    parser.error("fixture sizes must be positive")
trial = args.output.resolve() / ("run-" + str(time.time_ns()))
trial.mkdir(parents=True)


def git(path, *words):
    return subprocess.check_output(
        ["git", "-C", str(path), *words], stderr=subprocess.DEVNULL
    ).decode().strip()


def init(path):
    path.mkdir()
    git(path, "init", "-b", "main")
    git(path, "config", "user.name", "Benchmark")
    git(path, "config", "user.email", "benchmark@localhost")


def memory(pid):
    result = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith(("VmRSS:", "VmHWM:")):
            key, value, _ = line.split()
            result[key[:-1] + "_KiB"] = int(value)
    return result


def starts(trace):
    if not trace.exists():
        return 0
    return sum(json.loads(line).get("event") == "start"
               for line in trace.read_text().splitlines())


def launch(command, log_path, trace):
    with log_path.open("wb") as log:
        server = subprocess.Popen(command + ["daemon", "run"], stdout=log,
                                  stderr=log, env=dict(os.environ, GIT_TRACE2_EVENT=str(trace)))
    start = time.monotonic()
    try:
        while time.monotonic() - start < 300:
            response = subprocess.run(command + ["daemon", "status"], capture_output=True)
            if response.returncode == 0 and json.loads(response.stdout).get("ready"):
                return server, time.monotonic() - start
            if server.poll() is not None:
                raise RuntimeError(f"server exited: {log_path}")
            time.sleep(.02)
        raise RuntimeError("readiness timed out")
    except BaseException:
        server.terminate()
        server.wait(timeout=10)
        raise


def stop(command, server):
    subprocess.run(command + ["daemon", "stop", "--lazy"], capture_output=True)
    try:
        server.wait(timeout=10)
    except subprocess.TimeoutExpired:
        server.terminate()
        server.wait(timeout=10)


results = {}
for size_name, files in [("small", args.small_files), ("large", args.large_files)]:
    root = trial / size_name
    root.mkdir()
    seed, depot = root / "seed", root / "depot"
    init(seed)
    for i in range(files):
        path = seed / "tree" / f"d{i // 64:05}" / f"f{i:05}"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(f"{i:08}".encode() * 256)
    git(seed, "add", ".")
    git(seed, "commit", "-m", "fixture")
    pin = git(seed, "rev-parse", "HEAD")
    manifest = "format=1\n"
    for i in range(args.origins):
        origin = root / f"origin-{i}"
        git(root, "clone", "--bare", str(seed), str(origin))
        git(origin, "config", "uploadpack.allowFilter", "true")
        manifest += f'[repositories.r{i}]\nurl={json.dumps(origin.as_uri())}\n'
    for i in range(args.mounts):
        manifest += (f'[[mounts]]\nid="m{i}"\nrepository="r{i % args.origins}"\n'
                     f'commit="{pin}"\npath="projects/m{i:04}"\n')
    init(depot)
    (depot / "depot.toml").write_text(manifest)
    git(depot, "add", ".")
    git(depot, "commit", "-m", "layout")
    for label, binary in [("before", args.before), ("after", args.after)]:
        state = root / label
        command = [str(binary.resolve()), "--state-dir", str(state)]
        subprocess.run(command + ["--offline", "workspace", "create", "dev", "--from", str(depot)],
                       check=True, stdout=subprocess.DEVNULL)
        for name in ["indexes", "tree-cache"]:
            shutil.rmtree(state / name, ignore_errors=True)
        trace = root / f"{label}-trace.jsonl"
        server, elapsed = launch(command, root / f"{label}-server.log", trace)
        result = {"mounts": args.mounts, "files_per_mount": files,
                  "cold_start_seconds": elapsed, "startup_git_commands": starts(trace),
                  "startup_memory": memory(server.pid)}
        try:
            workspace = state / "mount/dev"
            target = workspace / "projects/m0000/tree/d00000"
            prior = starts(trace)
            begin = time.monotonic()
            result["first_listing_names"] = len(os.listdir(target))
            result["first_listing_seconds"] = time.monotonic() - begin
            result["first_listing_git_commands"] = starts(trace) - prior
            prior = starts(trace)
            begin = time.monotonic()
            subprocess.run(["ls", "-ln", "--color=never", str(target)],
                           check=True, stdout=subprocess.DEVNULL)
            result["ls_long_seconds"] = time.monotonic() - begin
            result["ls_long_git_commands"] = starts(trace) - prior
            prior = starts(trace)
            begin = time.monotonic()
            count = 0
            def walk_error(exc):
                raise exc
            for _, _, names in os.walk(workspace, followlinks=False, onerror=walk_error):
                count += len(names)
            result["full_walk_files"] = count
            result["full_walk_seconds"] = time.monotonic() - begin
            result["full_walk_git_commands"] = starts(trace) - prior
            result["after_walk_memory"] = memory(server.pid)
            status = json.loads(subprocess.check_output(command + ["status", "dev"]))
            result["tree_cache"] = status.get("tree_cache")
        finally:
            stop(command, server)
        server, result["cached_start_seconds"] = launch(command, root / f"{label}-warm.log", trace)
        stop(command, server)
        results[f"{size_name}-{label}"] = result
        (trial / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(size_name, label, json.dumps(result), flush=True)
    assert results[f"{size_name}-before"]["full_walk_files"] == results[f"{size_name}-after"]["full_walk_files"]
print("Results:", trial)
