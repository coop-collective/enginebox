"""Run on the admitted storage host, using only an explicitly owned test VM.

Archive extraction is a transport baseline, not a Lore/LayerFS implementation.
Inventory, UUID, host and fixture names are supplied privately, never in Git.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    p = argparse.ArgumentParser()
    for name in ['vm', 'project', 'uuid', 'owner', 'store']:
        p.add_argument('--'+name, required=True)
    a = p.parse_args()
    store = Path(a.store).resolve(strict=True)
    assert store.is_relative_to('/srv/enginebox/assets')
    assert (store/'owner').read_text().strip() == a.owner

    def incus(*argv, **kwargs):
        return subprocess.run(['incus', *argv], check=True, capture_output=True,
                              text=True, timeout=300, **kwargs)

    def guard():
        vm = json.loads(incus('query', f'/1.0/instances/{a.vm}?project={a.project}').stdout)
        assert vm['config']['volatile.uuid'] == a.uuid
        assert vm['config']['user.enginebox.experiment'] == a.owner
        assert vm['type'] == 'virtual-machine' and vm['status'] == 'Running'

    def guest(*argv, **kwargs):
        guard()
        return incus('exec', a.vm, '--project', a.project, '--', *argv, **kwargs).stdout

    def receipt(value):
        with (store/'restore-receipts.jsonl').open('a') as f:
            f.write(json.dumps({'at': time.time(), **value})+'\n')
            f.flush()
            os.fsync(f.fileno())
        print(json.dumps(value), flush=True)

    manifest = json.loads((store/'manifest.json').read_text())
    size = sum(x['bytes'] for x in manifest.values())
    archive = store/'content.tar.zst'
    assert archive.is_file() and not archive.is_symlink()
    start = time.monotonic()
    with archive.open('rb') as f:
        archive_hash = hashlib.file_digest(f, 'sha256').hexdigest()
        os.fsync(f.fileno())
        os.posix_fadvise(f.fileno(), 0, 0, os.POSIX_FADV_DONTNEED)
    receipt({'stage': 'archive', 'bytes': archive.stat().st_size,
             'sha256': archive_hash, 'verification_seconds': time.monotonic()-start,
             'content_bytes': size, 'files': len(manifest),
             'cache_boundary': 'Advisory eviction of this archive only; no global cache flush. Fresh guest target; other kernel/device caches uncontrolled.'})

    for mode in ['fresh', 'repeat']:
        dest = '/opt/unreal-storage-validation/'+mode
        preparation = '''import pathlib,shutil,json,sys
p=pathlib.Path(sys.argv[1]);n=int(sys.argv[2]);free=shutil.disk_usage('/').free
assert free>n+2*1024**3, 'guest disk reserve'
p.mkdir(parents=True,exist_ok=False)
print(json.dumps({'free_before':free}))'''
        capacity = json.loads(guest('python3', '-c', preparation, dest, str(size)))
        guard()
        started = time.monotonic()
        with (store/(mode+'-extract.log')).open('x') as log:
            decode = subprocess.Popen(['zstd', '-dq', '-c', str(archive)],
                                      stdout=subprocess.PIPE, stderr=log)
            extract = subprocess.Popen(['incus', 'exec', a.vm, '--project', a.project,
                                        '--', 'timeout', '--kill-after=5', '230',
                                        'tar', '-xpf', '-', '-C', dest],
                                       stdin=decode.stdout, stdout=log, stderr=log)
            decode.stdout.close()
            try:
                extract_code = extract.wait(timeout=240)
                decode_code = decode.wait(timeout=10)
            except subprocess.TimeoutExpired:
                extract.kill()
                decode.kill()
                extract.wait()
                decode.wait()
                receipt({'stage': mode, 'ok': False, 'reason': 'extraction timeout'})
                raise
            if extract_code or decode_code:
                receipt({'stage': mode, 'ok': False, 'codes': [extract_code, decode_code]})
                raise RuntimeError('extraction failed')
        extraction_seconds = time.monotonic()-started
        guest('python3', '-c', 'import os; os.sync()')
        durable_seconds = time.monotonic()-started
        verifier = '''import sys,json,pathlib,hashlib,time,shutil
expected=json.load(sys.stdin);root=pathlib.Path(sys.argv[1])/'EuropeanHornbeam';start=time.monotonic();actual={}
for p in sorted(root.rglob('*')):
 assert not p.is_symlink()
 if not p.is_file():continue
 with p.open('rb') as f:h=hashlib.file_digest(f,'sha256').hexdigest()
 actual[str(p.relative_to(root))]={'bytes':p.stat().st_size,'sha256':h}
assert actual==expected, 'full content manifest mismatch'
print(json.dumps({'hashes_match':True,'verification_seconds':time.monotonic()-start,'files':len(actual),'bytes':sum(x['bytes'] for x in actual.values()),'free_after':shutil.disk_usage('/').free}))'''
        verified = json.loads(guest('python3', '-c', verifier, dest,
                                    input=json.dumps(manifest)))
        receipt({'stage': mode, 'ok': True, **capacity, **verified,
                 'extraction_seconds': extraction_seconds,
                 'including_guest_sync_seconds': durable_seconds,
                 'method': 'same-host zstd archive piped through Incus into a fresh directory; no engine startup, Lore or LayerFS'})


if __name__ == '__main__':
    main()
