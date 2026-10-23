"""Compare sequential/parallel read-back with in-stream integrity verification.

Run on the storage host against a newly admitted, explicitly owned disposable VM.
No engine is launched and no fleet lifecycle/admission policy is implemented here.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    p = argparse.ArgumentParser()
    for key in ['vm', 'project', 'uuid', 'owner', 'store', 'output']:
        p.add_argument('--'+key, required=True)
    a = p.parse_args()
    store = Path(a.store).resolve(strict=True)
    output = Path(a.output)
    assert store.is_relative_to('/srv/enginebox/assets')
    assert output.parent.resolve(strict=True).is_relative_to('/srv/enginebox/assets')
    output.mkdir(mode=0o700)
    (output/'owner').write_text(a.owner+'\n')
    manifest = json.loads((store/'manifest.json').read_text())
    size = sum(x['bytes'] for x in manifest.values())
    archive = store/'content.tar.zst'
    assert archive.is_file() and not archive.is_symlink()
    helper = Path(__file__).with_name('verified-restore.py').read_text()
    guest_root = '/opt/unreal-verified-restore'

    def call(*argv, **kwargs):
        return subprocess.run(['incus', *argv], check=True, capture_output=True,
                              text=True, timeout=300, **kwargs)

    def guard():
        vm = json.loads(call('query', f'/1.0/instances/{a.vm}?project={a.project}').stdout)
        assert vm['config']['volatile.uuid'] == a.uuid
        assert vm['config']['user.enginebox.experiment'] == a.owner
        assert vm['status'] == 'Running' and vm['type'] == 'virtual-machine'

    def guest(*argv, **kwargs):
        guard()
        return call('exec', a.vm, '--project', a.project, '--', *argv, **kwargs).stdout

    def receipt(value):
        with (output/'receipts.jsonl').open('a') as f:
            f.write(json.dumps({'at': time.time(), **value})+'\n')
            f.flush()
            os.fsync(f.fileno())
        print(json.dumps(value), flush=True)

    preparation = '''import pathlib,shutil,sys,json
p=pathlib.Path(sys.argv[1]);size=int(sys.argv[2]);free=shutil.disk_usage('/').free
assert free>size*2+2*1024**3,'disk reserve for two copies'
p.mkdir(mode=0o700);data=json.load(sys.stdin)
(p/'helper.py').write_text(data['helper']);(p/'manifest.json').write_text(json.dumps(data['manifest']))
(p/'baseline').mkdir();print(json.dumps({'free_bytes':free}))'''
    capacity = json.loads(guest('python3', '-c', preparation, guest_root, str(size),
                               input=json.dumps({'helper': helper, 'manifest': manifest})))
    receipt({'stage': 'prepared', 'files': len(manifest), 'content_bytes': size, **capacity,
             'scope': 'same-host archive restore; no engine-start timing or deployed provider'})

    def pipeline(label, command):
        guard()
        with archive.open('rb') as f:
            os.fsync(f.fileno())
            os.posix_fadvise(f.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
        start = time.monotonic()
        with (output/(label+'.stderr')).open('x') as error:
            decode = subprocess.Popen(['zstd', '-dq', '-c', str(archive)],
                                      stdout=subprocess.PIPE, stderr=error)
            extract = subprocess.Popen(['incus', 'exec', a.vm, '--project', a.project,
                                        '--', 'timeout', '--kill-after=5', '240', *command],
                                       stdin=decode.stdout, stdout=subprocess.PIPE, stderr=error,
                                       text=True)
            decode.stdout.close()
            try:
                stdout, _ = extract.communicate(timeout=250)
                decode_code = decode.wait(timeout=10)
            except subprocess.TimeoutExpired:
                extract.kill()
                decode.kill()
                extract.wait()
                decode.wait()
                receipt({'stage': label, 'ok': False, 'reason': 'pipeline timeout'})
                raise
        assert extract.returncode == 0 and decode_code == 0, 'producer and extractor must both succeed'
        return time.monotonic()-start, stdout

    def audit(root, workers):
        return json.loads(guest('python3', guest_root+'/helper.py', 'audit',
                                '--manifest', guest_root+'/manifest.json',
                                '--destination', root, '--workers', str(workers)))

    duration, _ = pipeline('baseline', ['tar', '-xpf', '-', '-C', guest_root+'/baseline'])
    start = time.monotonic()
    guest('python3', '-c', 'import os; os.sync()')
    durable = duration + time.monotonic()-start
    base = guest_root+'/baseline/EuropeanHornbeam'
    # The baseline is an asset cache too: prevent ordinary consumer mutation.
    guest('python3', '-c', '''import os,sys
for current,dirs,files in os.walk(sys.argv[1]):
 for name in files:os.chmod(os.path.join(current,name),0o444)
 os.chmod(current,0o555)''', base)
    audits = []
    for workers in [1, 4, 1, 4]:
        r = audit(base, workers)
        audits.append(r)
        receipt({'stage': 'baseline-readback', **r})
    receipt({'stage': 'baseline-ready', 'extract_and_sync_seconds': durable,
             'serial_ready_seconds': durable+audits[0]['readback_seconds'],
             'readback_verified': True})

    duration, stdout = pipeline('inline', ['python3', guest_root+'/helper.py', 'restore',
                                           '--manifest', guest_root+'/manifest.json',
                                           '--destination', guest_root+'/inline'])
    restored = json.loads(stdout)
    root = restored.pop('content_root')
    # This receipt is the content-integrity boundary, before independent read-back.
    # The consumer must use a private fork; never write into this audited base.
    receipt({'stage': 'inline-ready', 'wall_seconds': duration, **restored,
             'engine_started': False, 'startup_can_be_requested_after_this_boundary': True})
    for workers in [4, 1, 4, 1]:
        receipt({'stage': 'inline-readback', **audit(root, workers)})


if __name__ == '__main__':
    main()
