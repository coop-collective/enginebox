"""Bounded local runner. Uses a fresh named-volume fixture, never a live Store."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

root = Path(__file__).resolve().parents[2]
private = root / '.state/layerfs-unreal'
stage = sys.argv[1]
identity = 'layerfs-unreal-20261023'
restoring = stage in ('restore-prepare', 'restore-read')
store_volume = 'ebox-lfu-restored-20261023' if restoring else 'ebox-lfu-store-20261023'
data_volume = 'ebox-lfu-data-20261023'
if stage == 'restore-prepare':
    found = subprocess.run(['docker', 'volume', 'inspect', store_volume], capture_output=True)
    if found.returncode == 0:
        raise ValueError('Restore destination must be a new volume')
    subprocess.run(['docker', 'volume', 'create', '--label',
                    f'enginebox.experiment={identity}', store_volume], check=True, capture_output=True)
for volume in (store_volume, data_volume):
    labels = json.loads(subprocess.check_output(['docker', 'volume', 'inspect', volume]))[0]['Labels']
    if labels.get('enginebox.experiment') != identity:
        raise ValueError(f'Refusing volume with different ownership: {volume}')
image = subprocess.check_output(['docker', 'image', 'inspect',
    'enginebox-layerfs-unreal:20261023', '--format', '{{.Id}}'], text=True).strip()
base = ['docker', 'run', '--rm', '--name', f'ebox-lfu-client-{stage}',
        '--label', f'enginebox.experiment={identity}', '--cpus', '2', '--memory', '3g',
        '-v', f'{root}/research/storage-benchmarks:/harness:ro',
        '-v', f'{data_volume}:/data', '-v', f'{store_volume}:/layerfs-store']
if stage == 'restore-prepare':
    script = '''import pathlib,shutil,hashlib,json,os
src=pathlib.Path('/data/unreal-sample-backup.sqlite')
dst=pathlib.Path('/layerfs-store/global/store.sqlite')
assert not dst.exists()
dst.parent.mkdir(mode=0o700)
shutil.copyfile(src,dst);dst.chmod(0o600)
def sha(p):
 h=hashlib.sha256()
 with p.open('rb') as f:
  for b in iter(lambda:f.read(1048576),b''): h.update(b)
 return h.hexdigest()
assert sha(src)==sha(dst)
home=pathlib.Path('/data/restore-home');home.mkdir(mode=0o700)
project=home/'unreal-sample';project.mkdir(mode=0o700)
shutil.copyfile('/data/home/unreal-sample/project.record',project/'project.record')
(project/'project.record').chmod(0o600)
print(json.dumps({'copiedBytes':dst.stat().st_size,'hashesMatch':True,'freshStoreVolume':True,'freshRuntimeDiscovery':True}))
'''
    base += [image, 'timeout', '110s', 'python3', '-c', script]
elif stage == 'prepare':
    original = Path(os.environ['UNREAL_PROBE_SOURCE']).resolve(strict=True)
    if not original.is_dir():
        raise ValueError('UNREAL_PROBE_SOURCE must be an environment asset directory')
    base += ['-v', f'{original}:/original:ro',
             image, 'timeout', '110s', 'python3', '/harness/layerfs-unreal-fixture.py',
             '/original', '/data/sample', 'sample']
else:
    base += ['-v', f'{private}/source:/src:ro',
             '-v', 'ebox-lfu-target-20261023:/artifacts:ro',
             '-v', '/var/run/docker.sock:/var/run/docker.sock']
    env = {'LAYERFS_HOME': '/data/restore-home' if restoring else '/data/home', 'LAYERFS_STORE_PATH': '/layerfs-store/global/store.sqlite',
           'LAYERFS_IMAGE': image, 'LAYERFS_VOLUME': store_volume,
           'LAYERFS_DAEMON': '/artifacts/release/layerfs-daemon',
           'LAYERFS_NATIVE': '/artifacts/release/layerfs.node',
           'LAYERFS_CONTAINER_PREFIX': 'ebox-lfu-restore-' if restoring else 'ebox-lfu-sample-',
           'LAYERFS_DAEMON_HOST': 'host.docker.internal',
           'LAYERFS_CONSTRUCTION_WORKERS': '1', 'UV_THREADPOOL_SIZE': '4'}
    for key, value in env.items(): base += ['-e', f'{key}={value}']
    base += [image, 'timeout', '--signal=TERM', '--kill-after=3s', '110s',
             'node', '/harness/layerfs-unreal.mjs', stage]
log_path = private / f'{stage}-{time.time_ns()}.log'
with log_path.open('x') as log:
    started = time.monotonic()
    p = subprocess.Popen(base, stdout=log, stderr=subprocess.STDOUT)
    try:
        status = p.wait(timeout=120)
    except subprocess.TimeoutExpired:
        subprocess.run(['docker', 'stop', '--time', '3', f'ebox-lfu-client-{stage}'],
                       timeout=10, capture_output=True)
        p.wait(timeout=10)
        status = 124
    receipt = {'stage': stage, 'status': status, 'seconds': time.monotonic()-started,
               'image': image, 'log': str(log_path), 'argv': base}
    with (private / 'invocations.jsonl').open('a') as receipts:
        receipts.write(json.dumps(receipt)+'\n')
print(log_path.read_text()[-12000:])
print(json.dumps({k:v for k,v in receipt.items() if k != 'argv'}))
sys.exit(status)
