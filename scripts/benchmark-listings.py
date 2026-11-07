#!/usr/bin/env python3
"""Persistent-process syscall benchmark using an isolated local Git/FUSE fixture.
Usage: benchmark-listings.py /path/to/gtl /workspace/scratch/metadata-benchmark
The destination must not exist. No network or existing mounts are used.
"""
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import time

binary = Path(sys.argv[1]).resolve()
root = Path(sys.argv[2]).resolve()
root.mkdir()
repo, state, mount = (root / name for name in ('origin', 'state', 'mount'))
repo.mkdir()
mount.mkdir()
def run(args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, **kwargs).stdout

def gtl(*args):
    return json.loads(run([str(binary), '--state-dir', str(state), '--json', *args]))

run(['git', 'init', '-b', 'main', str(repo)])
for directory, count in [('small', 100), ('large', 10000)]:
    (repo / directory).mkdir()
    for i in range(count):
        (repo / directory / f'file-{i:05d}').write_text('benchmark payload\n')
run(['git', '-C', str(repo), 'add', '.'])
run(['git', '-C', str(repo), '-c', 'user.name=Benchmark', '-c', 'user.email=benchmark@localhost', 'commit', '-m', 'fixture'])
mapfile = root / 'VERSION_MAP'
mapfile.write_text("format=1\n[[mount]]\nid='root'\nrepo='fixture'\npath=''\nrevision='main'\n")
gtl('repo', 'add', 'fixture', str(repo))
gtl('workspace', 'create', 'bench', '--map', str(mapfile))
gtl('activate', 'bench')
results = {}
with (root / 'daemon.log').open('w') as log:
    daemon = subprocess.Popen([str(binary), '--state-dir', str(state), 'mount', str(mount)], stdout=log, stderr=log)
    try:
        depot = mount / 'workspaces/bench/depot'
        for _ in range(200):
            if depot.is_dir():
                break
            if daemon.poll() is not None:
                raise RuntimeError('mount failed: ' + (root / 'daemon.log').read_text())
            time.sleep(.025)
        else:
            raise RuntimeError('mount timed out')
        def names(path):
            with os.scandir(path) as entries:
                return len(list(entries))
        def long(path):
            with os.scandir(path) as entries:
                return sum(e.stat(follow_symlinks=False).st_size for e in entries)
        for name, directory, listing in [('names100', 'small', names), ('names10000', 'large', names), ('long100', 'small', long)]:
            if listing == long:
                results['warmup'] = gtl('warmup', 'bench', 'small')
            path = depot / directory
            start = time.perf_counter_ns()
            listing(path)
            cold = (time.perf_counter_ns() - start) / 1e6
            before = gtl('metrics')
            samples = []
            for _ in range(100):
                start = time.perf_counter_ns()
                listing(path)
                samples.append((time.perf_counter_ns() - start) / 1e6)
            after = gtl('metrics')
            results[name] = {'first_ms': cold, 'median_ms': statistics.median(samples), 'p95_ms': sorted(samples)[94],
                             'counter_delta': {k: after[k] - before[k] for k in before if isinstance(before[k], int)}}
        print(json.dumps(results, indent=2), flush=True)
        (root / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
    finally:
        subprocess.run(['fusermount3', '-u', str(mount)], check=False)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.terminate()
            daemon.wait(timeout=10)
