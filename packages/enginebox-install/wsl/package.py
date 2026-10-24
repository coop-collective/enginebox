#!/usr/bin/env python3
import hashlib,pathlib,re,sys
output=pathlib.Path(sys.argv[1]);tag=sys.argv[2]
assert re.fullmatch(r'[a-zA-Z0-9_.-]+',tag)
image=output/'enginebox.wsl'
hashfile=lambda p:hashlib.file_digest(p.open('rb'),'sha256').hexdigest()
script=pathlib.Path(__file__).with_name('install.ps1.in').read_text()
script=script.replace('@@ROOTFS_SHA256@@',hashfile(image)).replace('@@ROOTFS_URL@@',f'https://github.com/coop-collective/enginebox/releases/download/{tag}/enginebox.wsl')
(output/'install.ps1').write_text(script)
(output/'SHA256SUMS').write_text(''.join(f'{hashfile(output/name)}  {name}\n' for name in ['install.ps1','enginebox.wsl']))
