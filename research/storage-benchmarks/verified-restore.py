"""Research: verify asset bytes during extraction; audit stored bytes separately.

Input is an uncompressed tar stream and a trusted {relative path: {bytes, sha256}}
manifest. This asset-only format excludes links, executable modes and ownership.
It is not a deployed Engine Box restore provider or an archive authentication API.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import sys
import tarfile
import time

BLOCK = 1024 * 1024


def parts(name):
    if (not isinstance(name, str) or not name or len(name) > 4096
            or '\\' in name or '\x00' in name or name.startswith('/')):
        raise ValueError('invalid relative path')
    raw = name.rstrip('/').split('/')
    if any(x in ['', '.', '..'] for x in raw) or len(raw) > 64:
        raise ValueError('invalid path component')
    return tuple(raw)


def validate_manifest(manifest):
    if not isinstance(manifest, dict) or not manifest or len(manifest) > 100000:
        raise ValueError('expected a bounded nonempty file manifest')
    for name, value in manifest.items():
        path = parts(name)
        if '/'.join(path) != name or not isinstance(value, dict):
            raise ValueError('noncanonical manifest path or entry')
        if (type(value.get('bytes')) is not int or value['bytes'] < 0
                or not isinstance(value.get('sha256'), str)
                or not re.fullmatch('[0-9a-f]{64}', value['sha256'])):
            raise ValueError('invalid size or SHA-256')
        if any('/'.join(path[:i]) in manifest for i in range(1, len(path))):
            raise ValueError('manifest file is also a parent directory')


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def restore(stream, manifest, destination, prefix, reserve_bytes=2 * 1024**3):
    validate_manifest(manifest)
    root_parts = parts(prefix)
    destination = Path(destination)
    total = sum(x['bytes'] for x in manifest.values())
    if shutil.disk_usage(destination.parent).free < total + reserve_bytes:
        raise ValueError('insufficient disk reserve')
    # Reserve a fresh private namespace; never replace an existing workspace.
    destination.mkdir(mode=0o700)
    staging = destination/'staging'
    staging.mkdir(mode=0o700)
    published = destination/'content'
    seen = set()
    directories = {staging}
    started = time.monotonic()

    def remember_directory(directory):
        while directory != staging:
            directories.add(directory)
            directory = directory.parent
        if len(directories) > len(manifest) * 16 + 32:
            raise ValueError('too many archive directories')

    try:
        with tarfile.open(fileobj=stream, mode='r|', bufsize=BLOCK) as archive:
            for entry in archive:
                path = parts(entry.name)
                if path[:len(root_parts)] != root_parts:
                    raise ValueError('entry outside expected archive prefix')
                relative = path[len(root_parts):]
                if entry.isdir():
                    directory = staging.joinpath(*relative)
                    directory.mkdir(parents=True, exist_ok=True)
                    remember_directory(directory)
                    continue
                if entry.type not in [tarfile.REGTYPE, tarfile.AREGTYPE] or not relative:
                    raise ValueError('only ordinary asset files are supported')
                name = '/'.join(relative)
                expected = manifest.get(name)
                if expected is None or name in seen or entry.size != expected['bytes']:
                    raise ValueError('unexpected, duplicate or wrong-sized file')
                target = staging.joinpath(*relative)
                target.parent.mkdir(parents=True, exist_ok=True)
                remember_directory(target.parent)
                h = hashlib.sha256()
                remaining = entry.size
                fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                with os.fdopen(fd, 'wb') as out, archive.extractfile(entry) as source:
                    while remaining:
                        data = source.read(min(BLOCK, remaining))
                        if not data:
                            raise ValueError('truncated asset')
                        h.update(data)
                        out.write(data)
                        remaining -= len(data)
                    if h.hexdigest() != expected['sha256']:
                        raise ValueError('asset checksum mismatch')
                    out.flush()
                    os.fchmod(out.fileno(), 0o444)
                    os.fsync(out.fileno())
                seen.add(name)
        # Drain the producer so the controller can inspect its exit status too.
        while stream.read(BLOCK):
            pass
        if seen != manifest.keys():
            raise ValueError('manifest files missing from archive')
        # Flush/seal directories deepest first before publishing a complete tree.
        for directory in sorted(directories, key=lambda p: len(p.parts), reverse=True):
            os.chmod(directory, 0o555)
            sync_directory(directory)
        staging.rename(published)
        sync_directory(destination)
        sync_directory(destination.parent)
        return {'content_verified': True, 'readback_verified': False,
                'files': len(seen), 'bytes': total,
                'restore_and_verify_seconds': time.monotonic()-started,
                'content_root': str(published)}
    finally:
        if staging.exists():
            # Only our reserved, unpublished staging tree is eligible for cleanup.
            for current, _, _ in os.walk(staging):
                os.chmod(current, 0o700)
            shutil.rmtree(staging)


def audit(root, manifest, workers=4):
    validate_manifest(manifest)
    if not 1 <= workers <= 8:
        raise ValueError('audit concurrency must be between 1 and 8')
    root = Path(root)
    if root.is_symlink() or not root.is_dir():
        raise ValueError('expected a real immutable content directory')
    started = time.monotonic()
    actual = set()
    for current, dirs, files in os.walk(root, followlinks=False):
        for name in dirs:
            if (Path(current)/name).is_symlink():
                raise ValueError('directory symlink refused')
        for name in files:
            path = Path(current)/name
            if path.is_symlink() or not path.is_file():
                raise ValueError('only regular files may be audited')
            actual.add(path.relative_to(root).as_posix())
    if actual != manifest.keys():
        raise ValueError('stored file set differs from manifest')

    def check(name):
        expected = manifest[name]
        fd = os.open(root/PurePosixPath(name), os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd, 'rb') as f:
            info = os.fstat(f.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_size != expected['bytes']:
                raise ValueError('stored file type/size mismatch')
            if hashlib.file_digest(f, 'sha256').hexdigest() != expected['sha256']:
                raise ValueError('stored checksum mismatch')

    with ThreadPoolExecutor(max_workers=workers) as pool:
        list(pool.map(check, sorted(manifest)))
    return {'readback_verified': True, 'workers': workers, 'files': len(actual),
            'bytes': sum(x['bytes'] for x in manifest.values()),
            'readback_seconds': time.monotonic()-started}


def main():
    p = argparse.ArgumentParser()
    p.add_argument('action', choices=['restore', 'audit'])
    p.add_argument('--manifest', required=True)
    p.add_argument('--destination', required=True)
    p.add_argument('--prefix', default='EuropeanHornbeam')
    p.add_argument('--workers', type=int, default=4)
    a = p.parse_args()
    manifest = json.loads(Path(a.manifest).read_text())
    result = (restore(sys.stdin.buffer, manifest, a.destination, a.prefix)
              if a.action == 'restore' else audit(a.destination, manifest, a.workers))
    print(json.dumps(result), flush=True)


if __name__ == '__main__':
    main()
