#!/usr/bin/env python3
"""Compare release binaries on an isolated, local, blob-filtered FUSE fixture."""
import argparse, json, os, pathlib, subprocess, time
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--before',type=pathlib.Path,required=True)
parser.add_argument('--after',type=pathlib.Path,required=True)
parser.add_argument('--output',type=pathlib.Path,required=True)
args=parser.parse_args()
root=args.output.resolve()
root.mkdir(parents=True,exist_ok=True)
trial=root / ('run-'+str(time.time_ns()))
trial.mkdir()
def git(p,*args):
    return subprocess.check_output(['git','-C',str(p),*args],stderr=subprocess.DEVNULL).decode().strip()
component=trial/'component';depot=trial/'depot'
for p in [component,depot]:
    p.mkdir();git(p,'init','-b','main');git(p,'config','user.name','Benchmark');git(p,'config','user.email','benchmark@localhost')
git(component,'config','uploadpack.allowFilter','true')
for i in range(600):
    (component/f'file-{i:04}').write_bytes((f'{i:04}'.encode())*512)
git(component,'add','.');git(component,'commit','-m','fixture')
pin=git(component,'rev-parse','HEAD')
(depot/'depot.toml').write_text(f'format=1\n[repositories.code]\nurl={json.dumps(str(component))}\n[[mounts]]\nid="code"\nrepository="code"\ncommit="{pin}"\npath="src"\n')
git(depot,'add','.');git(depot,'commit','-m','depot')
results={}
for label,binary in [('before',args.before.resolve()),('after',args.after.resolve())]:
    state=trial/label
    trace=trial/(label+'-trace.jsonl')
    cmd=[str(binary),'--state-dir',str(state)]
    subprocess.run(cmd+['--offline','workspace','create','dev','--from',str(depot)],check=True,stdout=subprocess.DEVNULL)
    with open(trial/(label+'-server.log'),'wb') as log:
        server=subprocess.Popen(cmd+['daemon','run'],stdout=log,stderr=log,env=dict(os.environ,GIT_TRACE2_EVENT=str(trace)))
    try:
        for attempt in range(200):
            r=subprocess.run(cmd+['daemon','status'],capture_output=True)
            if r.returncode==0 and json.loads(r.stdout).get('ready'):break
            if server.poll() is not None:raise RuntimeError('server exited')
            time.sleep(.05)
        else:raise RuntimeError('mount startup timeout')
        path=state/'mount/dev/src'
        def starts():
            if not trace.exists():return []
            return [e for line in trace.read_text().splitlines() if (e:=json.loads(line)).get('event')=='start']
        initial=len(starts())
        begin=time.monotonic()
        assert len(os.listdir(path))==603
        names=time.monotonic()-begin
        names_commands=len(starts())-initial
        prior=len(starts())
        begin=time.monotonic()
        subprocess.run(['ls','-ln','--color=never',str(path)],check=True,stdout=subprocess.DEVNULL)
        cold=time.monotonic()-begin
        cold_commands=starts()[prior:]
        prior=len(starts())
        begin=time.monotonic()
        subprocess.run(['ls','-ln','--color=never',str(path)],check=True,stdout=subprocess.DEVNULL)
        warm=time.monotonic()-begin
        warm_commands=len(starts())-prior
        cache_files=sum(p.is_file() for p in (state/'cache').rglob('*'))
        results[label]=dict(names_seconds=names,names_git_commands=names_commands,cold_seconds=cold,warm_seconds=warm,
            cold_git_commands=len(cold_commands),warm_git_commands=warm_commands,
            cold_fetches=sum('fetch' in e['argv'] for e in cold_commands),
            cold_cat_file=sum('cat-file' in e['argv'] for e in cold_commands),
            extracted_files=cache_files)
        print(label,json.dumps(results[label]),flush=True)
    finally:
        subprocess.run(cmd+['daemon','stop','--lazy'],capture_output=True)
        try:server.wait(timeout=10)
        except subprocess.TimeoutExpired:server.terminate();server.wait(timeout=10)
(trial/'results.json').write_text(json.dumps(results,indent=2)+'\n')
print('Results:',trial)
