#!/usr/bin/env python3
"""Package the reviewed native binary and installer under immutable URLs."""
import argparse
import hashlib
import json
import pathlib
import re
import shutil

parser = argparse.ArgumentParser()
parser.add_argument('--binary', type=pathlib.Path, required=True)
parser.add_argument('--output', type=pathlib.Path, required=True)
parser.add_argument('--tag', required=True)
parser.add_argument('--commit', required=True)
args = parser.parse_args()
assert re.fullmatch(r'linux-node-preview-[0-9]+', args.tag)
assert re.fullmatch(r'[a-f0-9]{40}', args.commit)
assert args.binary.is_file() and not args.binary.is_symlink()
args.output.mkdir(parents=True, exist_ok=True)
assert not list(args.output.iterdir()), 'Refusing to overwrite an existing package'
binary_hash = hashlib.sha256(args.binary.read_bytes()).hexdigest()
base = f'https://github.com/coop-collective/enginebox/releases/download/{args.tag}'
source = pathlib.Path(__file__).with_name('install.sh.in').read_text()
source = source.replace('@@NODE_BINARY_URL@@', base + '/enginebox-node').replace('@@NODE_BINARY_SHA256@@', binary_hash)
assert '@@' not in source
(args.output / 'install-linux-node.sh').write_text(source)
shutil.copyfile(args.binary, args.output / 'enginebox-node')
for name in ['install-linux-node.sh', 'enginebox-node']:
    (args.output / name).chmod(0o755)
(args.output / 'provenance.json').write_text(json.dumps({'source_commit': args.commit, 'target': 'x86_64-unknown-linux-gnu', 'role': 'managed-incus', 'binary_sha256': binary_hash}) + '\n')
paths = sorted(args.output.iterdir())
(args.output / 'SHA256SUMS').write_text(''.join(f'{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n' for p in paths))
