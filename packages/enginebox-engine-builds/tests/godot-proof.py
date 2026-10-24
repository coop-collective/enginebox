#!/usr/bin/env python3
"""Real installed Godot export/cache proof. Uses only a NEW owned proof directory.
The source here is a reproducible fixture, not a LayerFS integration claim.
Run browser-proof.mjs on the resulting web-receipt.json for actual Web playback.
"""
import argparse, hashlib, json, os, pathlib, shutil, subprocess, time
parser=argparse.ArgumentParser()
parser.add_argument('--godot',required=True,type=pathlib.Path)
parser.add_argument('--templates',required=True,type=pathlib.Path)
parser.add_argument('--binary',type=pathlib.Path,default=pathlib.Path('target/debug/enginebox-build'))
parser.add_argument('--out',required=True,type=pathlib.Path)
args=parser.parse_args()
base=args.out.resolve();base.mkdir(mode=0o700,parents=True,exist_ok=False)
root=base/'frozen';shutil.copytree(pathlib.Path(__file__).parent/'fixtures/godot',root)
def hash_file(path):
    with path.open('rb') as handle:
        return hashlib.file_digest(handle,'sha256').hexdigest()
def canonical(value):return json.dumps(value,sort_keys=True,separators=(',',':'),ensure_ascii=False)
def digest(value):return hashlib.sha256(canonical(value).encode()).hexdigest()
def write(name,value):
    path=base/name;path.write_text(canonical(value));path.chmod(0o600);return path
entries=[dict(path=p.relative_to(root).as_posix(),kind='file',sha256=hash_file(p),size=p.stat().st_size,executable=bool(p.stat().st_mode&0o111)) for p in sorted(root.rglob('*')) if p.is_file()]
source=dict(schema=1,project='build-proof',generation=digest(entries),layerfs=dict(revision='f'*40,project='build-proof',ref='fixture',root='fixture'),treeHash=digest(entries),entries=entries)
source_path=write('source.json',source)
version=subprocess.check_output([str(args.godot.resolve()),'--headless','--version'],text=True).strip()
results={}
for target,arch,template,entry in [('web','wasm32','web_nothreads_release.zip','index.html'),('linux','x86_64','linux_release.x86_64','game.x86_64')]:
    tool=args.godot.resolve();template_path=(args.templates/template).resolve()
    recipe=dict(schema=1,engine='godot',version=version,target=target,architecture=arch,executable=str(tool),executableSha256=hash_file(tool),inputs=[dict(name='export-template',path=str(template_path),sha256=hash_file(template_path))],entrypoint=entry,timeoutSeconds=300)
    recipe_path=write(target+'-recipe.json',recipe)
    command=[str(args.binary.resolve()),'build','--recipe',str(recipe_path),'--source',str(source_path),'--root',str(root),'--state',str(base/'state')]
    started=time.monotonic();first=json.loads(subprocess.check_output(command,text=True));first_ms=round((time.monotonic()-started)*1000)
    started=time.monotonic();second=json.loads(subprocess.check_output(command,text=True));second_ms=round((time.monotonic()-started)*1000)
    assert not first['cacheHit'] and second['cacheHit'] and first['artifactId']==second['artifactId']
    manifest=json.loads(pathlib.Path(first['manifestPath']).read_text());assert digest(manifest)==first['artifactId']
    for file in manifest['files']:assert hash_file(pathlib.Path(first['filesRoot'])/file['path'])==file['sha256']
    receipt=dict(**second,engineVersion=version,coldMs=first_ms,cachedMs=second_ms,files=len(manifest['files']),totalBytes=manifest['totalBytes'])
    write(target+'-receipt.json',receipt);results[target]=receipt
assert not (root/'.godot').exists() and not (root/'export_presets.cfg').exists()
write('receipt.json',results)
print(json.dumps(results,indent=2))
