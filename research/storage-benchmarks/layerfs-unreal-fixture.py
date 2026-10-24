"""Prepare private, Git-free storage inputs; never modify the original assets."""
import hashlib
import json
import shutil
import sys
import time
from pathlib import Path


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def manifest(root):
    result = {}
    for p in sorted(root.rglob('*')):
        if p.is_symlink():
            raise ValueError(f'Symlink refused: {p}')
        if p.is_file():
            result[p.relative_to(root).as_posix()] = {
                'bytes': p.stat().st_size, 'sha256': digest(p)}
    return result


if __name__ == '__main__':
    source, output = map(Path, sys.argv[1:3])
    mode = sys.argv[3] if len(sys.argv) > 3 else 'sample'
    if output.exists() or output.resolve().is_relative_to(source.resolve()):
        raise ValueError('Output must be fresh and outside the original assets')
    files = [p for p in source.rglob('*') if p.is_file()]
    if mode == 'full':
        selected = files
    elif mode == 'sample':
        meshes = sorted((p for p in files if '/Geometry/' in str(p)),
                        key=lambda p: p.stat().st_size, reverse=True)
        textures = sorted((p for p in files if '/Textures/' in str(p)),
                          key=lambda p: p.stat().st_size, reverse=True)
        maps = sorted((p for p in files if p.suffix == '.umap'),
                      key=lambda p: p.stat().st_size, reverse=True)
        selected = meshes[:1] + textures[:2] + maps[:1]
        if len(meshes) < 1 or len(textures) < 2:
            raise ValueError('Expected environment meshes and textures')
    else:
        raise ValueError('Mode must be sample or full')
    output.mkdir(parents=True)
    start = time.monotonic()
    original = {}
    for p in selected:
        if p.is_symlink():
            raise ValueError(f'Symlink refused: {p}')
        relative = p.relative_to(source)
        target = output / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(p, target)
        original[relative.as_posix()] = {'bytes': p.stat().st_size,
                                       'sha256': digest(p)}
    if mode == 'sample':
        shutil.copyfile(output / selected[0].relative_to(source),
                        output / 'duplicate-mesh.uasset')
        (output / 'agents').mkdir()
        for i in range(1000):
            (output / 'agents' / f'file-{i:04d}.txt').write_text(
                f'// synthetic small file {i}\n' + 'sample content\n' * 64)
        (output / 'task.json').write_text('{"version":1}\n')
    entries = manifest(output)
    for name, expected in original.items():
        assert entries[name] == expected
    report = {'mode': mode, 'files': len(entries),
              'bytes': sum(v['bytes'] for v in entries.values()),
              'prepare_and_verify_seconds': time.monotonic() - start,
              'cache_state': 'OS caches uncontrolled; preparation reads every input',
              'entries': entries, 'originals': original,
              'asset_validity': 'Original bytes verified; no Unreal load or resave performed'}
    output.with_suffix('.json').write_text(json.dumps(report, indent=2))
    print(json.dumps({k: v for k, v in report.items()
                      if k not in ('entries', 'originals')}))
