"""Bounded native UE cache qualification on private APFS copies.

No global cache eviction, live-workspace mutation, or cross-node timing claim.
Pass prepare SOURCE OUTPUT, then fill OUTPUT cold|warm|restored.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time

ENGINE = Path(os.environ.get('UNREAL_ENGINE', '/Users/Shared/Epic Games/UE_5.8'))
MAP = '/Game/EuropeanHornbeam/Maps/EuropeanHornbeam_Asset_Zoo'
MIN_FREE = 80 * 1024**3


def inventory(root, hashes=False):
    result = {}
    for p in sorted(root.rglob('*')):
        if p.is_symlink():
            raise ValueError(f'Symlink refused: {p}')
        if not p.is_file():
            continue
        s = p.stat()
        item = {'bytes': s.st_size}
        if hashes:
            h = hashlib.sha256()
            with p.open('rb') as f:
                for b in iter(lambda: f.read(4 * 1024**2), b''):
                    h.update(b)
            item['sha256'] = h.hexdigest()
        result[str(p.relative_to(root))] = item
    return result


def totals(root):
    values = inventory(root).values() if root.exists() else []
    return {'files': len(values), 'bytes': sum(x['bytes'] for x in values)}


def record(root, value):
    with (root/'receipts.jsonl').open('a') as f:
        f.write(json.dumps({'at': time.time(), **value})+'\n')
    print(json.dumps(value), flush=True)


def clone(src, dst):
    if dst.exists():
        raise ValueError('Destination must be fresh')
    subprocess.run(['/bin/cp', '-cR', str(src), str(dst)], check=True)


def prepare(source, output):
    if output.exists() or output.is_relative_to(source):
        raise ValueError('Output must be fresh and outside source')
    if shutil.disk_usage(output.parent).free < MIN_FREE:
        raise ValueError('Insufficient free disk for isolated test')
    output.mkdir(mode=0o700)
    project = output/'project'
    project.mkdir()
    started = time.monotonic()
    for name in ['Content', 'Config', 'Source', 'Plugins', 'Binaries']:
        if (source/name).exists():
            clone(source/name, project/name)
    files = list(source.glob('*.uproject'))
    assert len(files) == 1
    config = json.loads(files[0].read_text())
    disabled = []
    for plugin in config.get('Plugins', []):
        if plugin['Name'] in ['ModelContextProtocol', 'AllToolsets', 'MVVMToolset',
                              'SequencerAnimMixerToolset', 'LiveCodingToolset',
                              'ChaosClothAssetToolset', 'Fab', 'Bridge']:
            plugin['Enabled'] = False
            disabled.append(plugin['Name'])
    (project/files[0].name).write_text(json.dumps(config, indent=2)+'\n')
    # Private config only; no editor network service should start for a load probe.
    with (project/'Config/DefaultEngine.ini').open('a') as f:
        f.write('\n[/Script/AndroidFileServerEditor.AndroidFileServerRuntimeSettings]\n'
                'bEnablePlugin=False\nbAllowNetworkConnection=False\nSecurityToken=\n')
    clone_seconds = time.monotonic()-started
    original = inventory(source/'Content', hashes=True)
    copied = inventory(project/'Content', hashes=True)
    assert original == copied
    (output/'source-content-manifest.json').write_text(json.dumps(original))
    for d in ['ddc', 'logs']:
        (output/d).mkdir(mode=0o700)
    state = {'source': str(source), 'project_file': files[0].name, 'map': MAP,
             'disabled_plugins': disabled, 'cache': str(output/'ddc'),
             'engine_version': json.loads((ENGINE/'Engine/Build/Build.version').read_text()),
             'cache_boundary': 'Empty private filesystem DDC; OS/file/driver caches uncontrolled; installed engine and native project binaries reused',
             'materialization': 'APFS copy-on-write clone, not network download or physical full copy'}
    (output/'state.json').write_text(json.dumps(state, indent=2))
    record(output, {'stage': 'prepare', 'ok': True, 'clone_seconds': clone_seconds,
                    'including_content_hash_verification_seconds': time.monotonic()-started,
                    'content_files': len(original), 'content_bytes': sum(x['bytes'] for x in original.values()),
                    'content_hashes_match': True, 'project': totals(project),
                    'ddc': totals(output/'ddc'), 'disabled_plugins': disabled})


def fill(root, mode):
    state = json.loads((root/'state.json').read_text())
    if mode in ['warm', 'restored']:
        prior = [json.loads(s) for s in (root/'receipts.jsonl').read_text().splitlines()]
        assert any(r.get('stage') == 'cold' and r.get('exit_code') == 0
                   and r.get('commandlet_completed') and r.get('map_processed')
                   for r in prior), 'cold fill must finish first'
    runtime = state.setdefault('runtime', {
        'disable_app_nap': os.environ.get('UNREAL_PROBE_DISABLE_APP_NAP') == '1',
        'engine_pak': os.environ.get('UNREAL_PROBE_ENGINE_PAK') == '1',
        'null_rhi': os.environ.get('UNREAL_PROBE_NULL_RHI') == '1',
    })
    (root/'state.json').write_text(json.dumps(state, indent=2))
    project = root/'project'
    cache = root/'ddc'
    if mode == 'restored':
        # Orderly stop precedes this copy. Runtime state is disposable; preserve
        # imported/build products and file DDC, never locks or a live Zen database.
        started = time.monotonic()
        restored = root/'restored'
        restored.mkdir(mode=0o700)
        clone(project, restored/'project')
        clone(cache, restored/'ddc')
        before = inventory(cache, hashes=True)
        assert inventory(restored/'ddc', hashes=True) == before
        project, cache = restored/'project', restored/'ddc'
        record(root, {'stage': 'restore-cache', 'seconds': time.monotonic()-started,
                      'ddc': totals(cache), 'hashes_match': True,
                      'copy': 'APFS clone to a new path, no network transfer'})
    elif mode == 'cold':
        assert totals(cache)['files'] == 0
    elif mode != 'warm':
        raise ValueError('Expected cold, warm or restored')
    editor = ENGINE/'Engine/Binaries/Mac/UnrealEditor.app/Contents/MacOS/UnrealEditor'
    log = root/'logs'/f'{mode}-{time.time_ns()}.log'
    # The DDC commandlet treats Map as a filename wildcard, not a /Game package.
    # NormalizePackageNames adds the content roots and the commandlet adds .umap.
    map_relative = state['map'].removeprefix('/Game/')
    assert (project/'Content'/(map_relative+'.umap')).is_file(), 'map must exist'
    args = [str(editor), str(project/state['project_file']), '-run=DerivedDataCache',
            '-fill', f'-Map={map_relative}', '-TargetPlatform=Mac',
            '-AllowCommandletRendering', '-RenderOffscreen', '-unattended', '-nop4',
            '-nosplash', '-nosound', '-NoLiveCoding', '-stdout', '-FullStdOutLogOutput',
            f'-abslog={log.with_suffix(".unreal.log")}',
            f'-DDC=(Probe=(Type=FileSystem,Path="{cache}",UnusedFileAge=34,DeleteUnused=false))',
            '-ini:Engine:[DevOptions.Shaders]:ShaderCompilerCoreCountThreshold=999',
            '-ini:Engine:[DevOptions.Shaders]:NumUnusedShaderCompilingThreads=14',
            '-ini:Engine:[DevOptions.Shaders]:NumUnusedShaderCompilingThreadsDuringGame=14']
    # Per-process diagnostic only; do not alter the installed engine or defaults.
    # Record this separately from an unmodified launch, including unsuccessful runs.
    if runtime['disable_app_nap']:
        args.extend(['-NSAppSleepDisabled', 'YES'])
    if runtime['engine_pak']:
        args = [s.replace('-DDC=(Probe=', '-DDC=(EnginePak=InstalledEnginePak,Probe=')
                if s.startswith('-DDC=') else s for s in args]
    if runtime.get('null_rhi'):
        args = [s for s in args if s not in ['-AllowCommandletRendering', '-RenderOffscreen']]
        args.append('-NullRHI')
    before = totals(cache)
    started = time.monotonic()
    deadline = started + int(os.environ.get('UNREAL_PROBE_TIMEOUT', '900'))
    with log.open('x') as out:
        p = subprocess.Popen(args, stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
        (root/'active.json').write_text(json.dumps({'pid': p.pid, 'mode': mode, 'log': str(log)}))
        reason = None
        while p.poll() is None:
            if time.monotonic() > deadline:
                reason = 'deadline'
            elif shutil.disk_usage(root).free < MIN_FREE:
                reason = 'disk-floor'
            if reason:
                os.killpg(p.pid, signal.SIGTERM)
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(p.pid, signal.SIGKILL)
                    p.wait(timeout=10)
                break
            time.sleep(1)
    text = log.read_text(errors='replace')
    report = {'stage': mode, 'seconds': time.monotonic()-started, 'exit_code': p.returncode,
              'stop_reason': reason, 'ddc_before': before, 'ddc_after': totals(cache),
              'project_generated': {n: totals(project/n) for n in ['Intermediate','DerivedDataCache','Saved']},
              'commandlet_completed': 'Success - 0 error(s)' in text,
              'map_processed': f'Processing {state["map"]}' in text,
              'no_packages_found': 'No packages found to load from command line arguments.' in text,
              'log': str(log), 'argv': args}
    record(root, report)
    (root/'active.json').unlink()
    return 0 if (p.returncode == 0 and report['commandlet_completed']
                 and report['map_processed'] and not report['no_packages_found']) else 1


if __name__ == '__main__':
    if sys.argv[1] == 'prepare':
        prepare(Path(sys.argv[2]).resolve(strict=True), Path(sys.argv[3]).resolve())
    elif sys.argv[1] == 'fill':
        sys.exit(fill(Path(sys.argv[2]).resolve(strict=True), sys.argv[3]))
    else:
        raise ValueError('Expected prepare or fill')
