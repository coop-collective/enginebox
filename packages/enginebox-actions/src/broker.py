"""One-use guest-root handoff. No network, host socket, credentials or publication authority."""
import json
import os
import pathlib
import re
import sys
import time


def read_json(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as stream:
        content = stream.read(262145)
    assert len(content) <= 262144, 'Oversized preparation request'
    return json.loads(content)


def directory_at(parent, parts):
    fd = os.dup(parent)
    try:
        for part in parts:
            assert part not in ['', '.', '..'] and '/' not in part
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def serve(control, seconds):
    control = pathlib.Path(control)
    policy = read_json(control / 'policy.json')
    root = pathlib.Path(policy['root'])
    private = control / 'private/source'
    # Bootstrap supplies a root-only view and an independently read-only public bind.
    mount = next(line.split() for line in pathlib.Path('/proc/self/mountinfo').read_text().splitlines() if line.split()[4] == str(root / 'prepared'))
    assert 'ro' in mount[5].split(',')
    assert os.getuid() == 0 and (control / 'private').stat().st_mode & 0o077 == 0
    deadline = time.monotonic() + seconds
    request = None
    while time.monotonic() < deadline:
        if (control / 'requests/restore.json').exists() and (control / 'verified.json').exists():
            request = read_json(control / 'requests/restore.json')
            break
        time.sleep(0.01)
    if request is None:
        return
    reply = {'ok': False, 'request': request.get('request')}
    try:
        assert re.fullmatch(r'[a-f0-9-]{36}', request['request'])
        assert re.fullmatch(r'[a-f0-9]{64}', request['key'])
        assignment = read_json(control / 'verified.json')
        assert assignment['verified'] is True and assignment['repository_id'] == policy['repository_id']
        source = pathlib.Path(request['source'])
        parts = source.relative_to(root).parts
        assert len(parts) >= 3 and parts[0] == 'work' and str(source) == request['inputs']['workspace']
        key = request['key']
        manifest = read_json(private / 'prepared' / key / 'manifest.json')
        assert manifest['key'] == key and manifest['inputs'] == request['inputs']
        assert manifest['inputs']['policy'] == policy['recipe'] and policy['recipe']['scripts'] == 'deny'
        view = os.open(private, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        src = dest = None
        try:
            src = directory_at(view, ['prepared', key])
            dest = directory_at(view, parts)
            # Refuse even an existing dangling symlink. Never overwrite a job's install.
            try:
                os.stat('node_modules', dir_fd=dest, follow_symlinks=False)
            except FileNotFoundError:
                pass
            else:
                raise AssertionError('Destination already contains dependencies')
            module = os.open('node_modules', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=src)
            os.close(module)
            os.rename('node_modules', 'node_modules', src_dir_fd=src, dst_dir_fd=dest)
        finally:
            for fd in [src, dest, view]:
                if fd is not None:
                    os.close(fd)
        reply.update(ok=True, key=key, validation='protected-generation')
    except Exception:
        reply['error'] = 'Approved dependency handoff failed'
    target = control / 'restored.tmp'
    with target.open('x') as stream:
        json.dump(reply, stream)
    target.chmod(0o444)
    target.rename(control / 'restored.json')


if __name__ == '__main__':
    serve(sys.argv[1], min(21800, max(1, int(sys.argv[2]))))
