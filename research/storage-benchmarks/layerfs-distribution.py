"""Bounded loopback distribution probe of an already consistent SQLite snapshot.

Reads a private snapshot, never a live Store. desync v1.1.5 must be provisioned
and verified separately. Output is private research evidence, not a product cache.
"""
import argparse
import functools
import hashlib
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import threading
import time


def digest(path):
    with path.open('rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()


def check_sqlite(path):
    with sqlite3.connect(path.as_uri()+'?mode=ro&immutable=1', uri=True) as db:
        assert db.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
        assert not db.execute('PRAGMA foreign_key_check').fetchall()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--snapshot', type=Path, required=True)
    parser.add_argument('--desync', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    source = args.snapshot.resolve(strict=True)
    binary = args.desync.resolve(strict=True)
    output = args.output.absolute()
    assert source.is_file() and not args.snapshot.is_symlink()
    assert not output.exists() and output.parent.is_dir()
    assert not any(Path(str(source)+s).exists() for s in ['-wal', '-shm', '-journal'])
    assert shutil.disk_usage(output.parent).free > source.stat().st_size*6+1024**3
    output.mkdir(mode=0o700)
    (output/'owner').write_text('layerfs-distribution-research-v1\n')
    config = output/'desync.json'
    config.write_text('{}\n')  # Never load the operator's normal remote config.
    report = {'scope': 'loopback HTTP; immutable snapshot; no live LayerFS mount',
              'snapshot_bytes': source.stat().st_size, 'runs': []}
    expected = digest(source)
    check_sqlite(source)

    def run(label, *argv, success=True):
        start = time.monotonic()
        p = subprocess.run([str(binary), '--config', str(config), *map(str, argv)],
                           capture_output=True, text=True, timeout=180)
        elapsed = time.monotonic()-start
        (output/(label+'.stdout')).write_text(p.stdout)
        (output/(label+'.stderr')).write_text(p.stderr)
        if (p.returncode == 0) != success:
            (output/'failure.json').write_text(json.dumps({
                'stage': label, 'seconds': elapsed, 'exit_code': p.returncode})+'\n')
        assert (p.returncode == 0) == success, f'{label}: unexpected exit {p.returncode}'
        return {'stage': label, 'seconds': elapsed, 'exit_code': p.returncode}

    def record(row):
        report['runs'].append(row)
        (output/'report.json').write_text(json.dumps(report, indent=2)+'\n')
        print(json.dumps(row), flush=True)

    version = subprocess.check_output([str(binary), '--version'], text=True).strip()
    assert version == 'desync version v1.1.5'
    report['tool'] = version
    store = output/'store'
    cache = output/'cache'
    store.mkdir()
    cache.mkdir()
    index = output/'store.sqlite.caibx'
    record(run('publish', 'make', '-n', 4, '-s', store, index, source))
    report['chunk_files'] = sum(p.is_file() for p in store.rglob('*'))
    report['stored_file_bytes'] = sum(p.stat().st_size for p in store.rglob('*') if p.is_file())
    counts = {'requests': 0, 'bytes': 0}
    counter_lock = threading.Lock()

    class Handler(SimpleHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'
        disable_nagle_algorithm = True

        def log_message(self, *_):
            pass

        def do_GET(self):
            with counter_lock:
                counts['requests'] += 1
            super().do_GET()

        def copyfile(self, source_file, target):
            while data := source_file.read(1024*1024):
                target.write(data)
                with counter_lock:
                    counts['bytes'] += len(data)

    class Server(ThreadingHTTPServer):
        request_queue_size = 64

    server = Server(('127.0.0.1', 0), functools.partial(Handler, directory=str(store)))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f'http://127.0.0.1:{server.server_port}/'

    def extract(label, repair=False, success=True):
        target = output/(label+'.sqlite')
        before = dict(counts)
        row = run(label, 'extract', '-n', 4, '--error-retry', 0,
                  '--cache-repair='+str(repair).lower(), '-s', url, '-c', cache,
                  index, target, success=success)
        row.update({k: counts[k]-before[k] for k in counts})
        if success:
            started = time.monotonic()
            assert digest(target) == expected
            check_sqlite(target)
            row.update({'sha256_passed': True, 'sqlite_checks_passed': True,
                        'verification_seconds': time.monotonic()-started})
            # Delete only this fresh test's verified output, retaining chunks.
            target.unlink()
        record(row)
        return row

    try:
        extract('cold')
        warm = extract('warm')
        assert warm['requests'] == 0 and warm['bytes'] == 0
        chunk = next(p for p in cache.rglob('*.cacnk') if p.is_file())
        original = chunk.read_bytes()
        chunk.write_bytes(b'not-a-valid-compressed-chunk')
        extract('corrupt-cache-refused', success=False)
        repaired = extract('cache-repaired', repair=True)
        assert repaired['requests'] > 0 and chunk.read_bytes() == original
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)

    offline = extract('origin-offline')
    assert offline['requests'] == 0
    assert digest(source) == expected
    report['original_unchanged'] = True
    report['limitations'] = [
        'No WAN, separate Linux kernel, engine launch, concurrent publisher or production service.',
        'No changed-generation transfer or LayerFS runtime format migration measured.',
        'Checks follow extraction; no consumer sees a partly restored tree in this probe.',
        'Normal OS caches; four transfer workers; timings are individual observations.',
        'Project-aware eviction, controller receipts and authenticated manifests are not implemented.'
    ]
    (output/'report.json').write_text(json.dumps(report, indent=2)+'\n')


if __name__ == '__main__':
    main()
