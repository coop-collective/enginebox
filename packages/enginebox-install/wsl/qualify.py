#!/usr/bin/env python3
"""Real disposable LayerFS proof in the owned Linux runtime; no model credentials."""
import json, pathlib, subprocess, uuid
root=pathlib.Path('/var/lib/enginebox')
receipt=root/'qualification.json'
base=['/usr/local/bin/ebox','storage','layerfs','--config','/etc/enginebox/layerfs.json']
def call(*args):
    p=subprocess.run(base+list(args),text=True,capture_output=True,timeout=300)
    if p.returncode: raise RuntimeError(p.stderr[-2000:])
    r=json.loads(p.stdout)
    if r.get('ok') is False: raise RuntimeError('LayerFS operation reported failure: '+json.dumps(r)[-2000:])
    return r
call('doctor')
settings=json.loads(pathlib.Path('/etc/enginebox/settings.json').read_text())
cpu=pathlib.Path('/sys/fs/cgroup/enginebox.slice/cpu.max').read_text().split()
assert int(cpu[0]) <= settings['cpu']*int(cpu[1])
assert int(pathlib.Path('/sys/fs/cgroup/enginebox.slice/memory.max').read_text()) <= settings['memory_gib']*1024**3
if receipt.exists():
    proof=json.loads(receipt.read_text())
    assert proof['layerfs'] and proof['isolation']
    print('Existing runtime qualification and current prerequisites verified.')
    raise SystemExit(0)
seed=pathlib.Path('/opt/enginebox/seed')
# Connector-only import staging. The resulting normal workspace has no .git.
if not (seed/'.git').exists():
    subprocess.run(['git','init','-q',str(seed)],check=True)
    subprocess.run(['git','-C',str(seed),'add','README.md'],check=True)
    subprocess.run(['git','-C',str(seed),'-c','user.name=Enginebox','-c','user.email=build@localhost','commit','-qm','Credential-free welcome project'],check=True)
projects=call('list')
# Partial/uncertain imports are deliberately not replayed: LayerFS refuses an
# existing project receipt and tells the operator to reconcile it.
if not (root/'layerfs/projects/welcome/project.json').exists():
    preview=call('preview','--repo',str(seed))
    call('import','welcome','--repo',str(seed),'--revision',preview['commit'],'--approval',preview['approval'],'--credential-free','--source-only')
suffix=uuid.uuid4().hex[:12]
a,b='proof-a-'+suffix,'proof-b-'+suffix
call('fork','welcome',a); call('fork','welcome',b)
for workspace in [a,b]:
    w=json.loads((root/'layerfs/workspaces'/f'{workspace}.json').read_text())
    inspected=json.loads(subprocess.check_output(['docker','--host','unix:///run/enginebox-docker.sock','inspect',w['container']],text=True))[0]
    assert inspected['HostConfig']['CgroupParent']=='enginebox.slice'
    assert inspected['HostConfig']['Memory'] > 0
    assert not inspected['HostConfig'].get('Privileged')
canary=root/'private-canary';canary.write_text(uuid.uuid4().hex);canary.chmod(0o600)
try:
    script='set -eu; test "$(id -u)" != 0; test ! -e /mnt/c; test ! -r /var/lib/enginebox/private-canary; test ! -S /run/enginebox-docker.sock; test ! -S /var/run/docker.sock; test ! -r /layerfs-store/global/store.sqlite; test ! -e /project/.git; codex --version; claude --version; printf private-a > proof.txt; mkdir -p "$HOME/.codex"; printf private-credential > "$HOME/.codex/proof"'
    call('exec',a,'--timeout','90','--','bash','-c',script)
    call('exec',b,'--timeout','30','--','bash','-c','test ! -e proof.txt && test ! -e "$HOME/.codex/proof"')
    call('checkpoint',a,'--message','Disposable workspace proof')
    call('remount',a)
    call('exec',a,'--timeout','30','--','bash','-c','test "$(cat proof.txt)" = private-a')
    call('checkpoint',a);call('checkpoint',b);call('close',a);call('close',b)
    receipt.write_text(json.dumps({'layerfs':True,'isolation':True,'checkpoint_remount':True,'providers_installed':['codex','claude'],'live_inference':False}))
    print('LayerFS fork, private workspace files/credentials, host-access refusal and checkpoint/remount passed.')
finally:
    canary.unlink(missing_ok=True)
