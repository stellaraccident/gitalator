#!/usr/bin/env python3
"""Reproducible recursive import benchmark; creates isolated local fixtures only."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
p.add_argument('--mounts', type=int, default=679)
p.add_argument('--origins', type=int, default=32)
p.add_argument('--failure-delay', type=float, default=0.05)
a = p.parse_args()
assert a.mounts > a.origins + 1 and a.origins > 0
root = a.output.resolve() / f'run-{time.time_ns()}'
root.mkdir(parents=True)
binary = a.binary.resolve()


def git(repo, *args, data=None):
    return subprocess.check_output(['git', '-C', str(repo), *args], input=data, stderr=subprocess.DEVNULL).decode().strip()


def links(repo, children):
    (repo / '.gitmodules').write_text(''.join(f'[submodule "m{i}"]\npath = {path}\nurl = {url}\n' for i, (path, url, _) in enumerate(children)))
    git(repo, 'add', '.gitmodules')
    git(repo, 'update-index', '--index-info', data=''.join(f'160000 {pin}\t{path}\n' for path, _, pin in children).encode())
    git(repo, 'commit', '-m', 'forest')
    return git(repo, 'rev-parse', 'HEAD')


seed = root / 'seed'
seed.mkdir()
git(seed, 'init', '-b', 'main')
git(seed, 'config', 'user.name', 'Benchmark')
git(seed, 'config', 'user.email', 'benchmark@localhost')
(seed / 'file').write_text('payload\n')
git(seed, 'add', 'file')
git(seed, 'commit', '-m', 'leaf')
leaf_pin = git(seed, 'rev-parse', 'HEAD')
root_links = []
leaves = a.mounts - a.origins - 1
for i in range(a.origins):
    repo = root / f'origin-{i}'
    git(root, 'clone', str(seed), str(repo))
    git(repo, 'config', 'user.name', 'Benchmark')
    git(repo, 'config', 'user.email', 'benchmark@localhost')
    count = leaves // a.origins + (i < leaves % a.origins)
    pin = links(repo, [(f'deps/leaf{n:04}', repo, leaf_pin) for n in range(count)])
    root_links.append((f'branch{i:04}', repo, pin))
links(seed, root_links)
failed = root / 'failed'
git(root, 'clone', str(seed), str(failed))
git(failed, 'config', 'user.name', 'Benchmark')
git(failed, 'config', 'user.email', 'benchmark@localhost')
git(failed, 'reset', '--hard', leaf_pin)
links(failed, [(f'broken{i:04}', f'ssh://example.invalid/repo{i}', f'{i+1:040x}') for i in range(a.mounts-1)])
helper = root / 'ssh-helper'
helper.write_text(f'#!/bin/sh\necho "$*" >> "$IMPORT_PROBES"\nsleep {a.failure_delay}\necho "Permission denied (publickey)." >&2\nexit 255\n')
results = []
for jobs in [1, 32]:
    for failure in [False, True]:
        state = root / f'state-{jobs}-{failure}'
        base = [str(binary), '--state-dir', str(state), '--offline']
        subprocess.run(base + ['workspace', 'init', 'example'], check=True, stdout=subprocess.DEVNULL)
        for phase in ['cold', 'hot'] if not failure else ['failures']:
            trace = root / f'trace-{jobs}-{phase}.jsonl'
            probes = root / f'probes-{jobs}-{phase}'
            env = dict(os.environ, GTL_SUBMODULE_JOBS=str(jobs), GIT_TRACE2_EVENT=str(trace), GIT_SSH_COMMAND=f'sh "{helper}"', GIT_SSH_VARIANT='ssh', IMPORT_PROBES=str(probes))
            args = ['mount', 'submodules', '--depth', 'recursive'] if phase == 'hot' else ['mount', 'add', str(failed if failure else seed), '--submodules', 'recursive']
            args += ['--at', '.', '--workspace', 'example', '--skip-unavailable']
            rss = root / f'rss-{jobs}-{phase}'
            start = time.monotonic()
            out = subprocess.run(['/usr/bin/time', '-f', '%M', '-o', str(rss), *base, *args], env=env, capture_output=True, check=True)
            seconds = time.monotonic()-start
            report = json.loads(out.stdout)
            events = [json.loads(line) for line in trace.read_text().splitlines()] if trace.exists() else []
            commands = [e for e in events if e.get('event') == 'start']
            fetches = sum('fetch' in e.get('argv',[]) for e in commands)
            # Trace2 start/exit lifetimes measure overlapping Git processes.
            running = peak = 0
            for event in sorted(events, key=lambda e: e.get('time','')):
                running += (event.get('event') == 'start') - (event.get('event') == 'exit')
                peak = max(peak, running)
            result = dict(jobs=jobs, phase=phase, seconds=seconds, git_commands=len(commands), fetches=fetches, peak_git_processes=peak, max_rss_kib=int(rss.read_text()), network_probes=len(probes.read_text().splitlines()) if probes.exists() else 0, skipped=len(report['skipped']))
            results.append(result)
            print(json.dumps(result), flush=True)
            (root / 'results.json').write_text(json.dumps(results, indent=2)+'\n')
maps = [json.loads(next((root / f'state-{jobs}-False/workspaces').glob('*/workspace.json')).read_text())['map'] for jobs in [1, 32]]
assert maps[0] == maps[1], 'serial and parallel layouts differ'
assert len(maps[0]['mounts']) == a.mounts
print(root / 'results.json')
