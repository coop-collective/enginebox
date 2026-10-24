"""Opt-in disposable GitHub/LayerFS development qualification.
Run on a provisioned Linux node with explicit fixture repository and seed commit.
Never runs against a developer workspace or invents approval for arbitrary code.
"""
import argparse,json,os,pathlib,subprocess
p=argparse.ArgumentParser();p.add_argument('phase',choices=['prepare','promote']);p.add_argument('--ebox',required=True);p.add_argument('--config',required=True);p.add_argument('--credential',required=True);p.add_argument('--repository',required=True);p.add_argument('--seed',required=True);p.add_argument('--report',required=True);a=p.parse_args()
assert os.environ.get('ENGINEBOX_LIVE_GITHUB_DEV')=='1','Explicit live qualification opt-in required'
report=pathlib.Path(a.report);prefix=[a.ebox,'--host','local','storage','layerfs','--config',a.config]
def run(*args):
    result=subprocess.run(prefix+list(args),capture_output=True,text=True,timeout=600)
    if result.returncode:raise RuntimeError(result.stderr[-3000:])
    value=json.loads(result.stdout)
    assert value.get("ok") is not False, f"Operation failed: {args[0]}"
    if args[0]=="exec": assert value["result"]["exitCode"]==0, "Application command failed"
    print(json.dumps({'operation':args[0],'result':value}),flush=True)
    return value
if a.phase=='prepare':
    assert not report.exists(),'Use a fresh fixture/report; do not replay uncertain operations'
    review=run('github-preview','live-app','--repository',a.repository,'--branch','main','--token-file',a.credential)
    expected={'.gitignore','README.md','build.mjs','package.json','server.mjs','src/index.html','src/release.json','test.mjs'}
    assert review['commit']==a.seed and set(review['files'])==expected and review['bytes']<100000
    imported=run('github-import','live-app','--revision',review['commit'],'--approval',review['approval'],'--credential-free')
    run('github-fork','live-app','live-preview-v2')
    run('exec','live-preview-v2','--','sh','-c','test ! -e /project/.git && npm test && npm run build')
    edit="const fs=require('fs');const p='src/release.json';const r=JSON.parse(fs.readFileSync(p));r.version='2';r.headline='Built here. Ready to ship.';r.message='This change was made and tested in a Git-free LayerFS workspace. Previewed before promotion, then synchronized back to GitHub.';fs.writeFileSync(p,JSON.stringify(r)+'\\n');"
    run('exec','live-preview-v2','--','node','-e',edit)
    tested=run('exec','live-preview-v2','--','sh','-c','test ! -e .git && npm test && npm run build')
    checkpoint=run('checkpoint','live-preview-v2','--message','Build and test app version 2')
    run('close','live-preview-v2')
    main=run('dev-export','live-app');preview=run('dev-export','live-app','--workspace','live-preview-v2')
    for snapshot,version in [(main,"1"),(preview,"2")]:
        assert len(snapshot["manifest"]["entries"])>=8, "Source export omitted the mounted checkout"
        assert json.loads((pathlib.Path(snapshot["sourceRoot"])/"src/release.json").read_text())["version"]==version
    report.parent.mkdir(parents=True,exist_ok=True)
    report.write_text(json.dumps({'phase':'prepared','seed':a.seed,'repository':a.repository,'import':imported,'checkpoint':checkpoint,'main_export':main,'preview_export':preview,'tests':tested},indent=2))
    report.chmod(0o600)
else:
    saved=json.loads(report.read_text());assert saved['phase']=='prepared' and saved['seed']==a.seed
    settings=run('github-settings','live-app','--enabled','true')
    result=run('github-promote','live-app','live-preview-v2','--expected-main',settings['main']['revision'],'--message','Ship the tested LayerFS app revision','--token-file',a.credential)
    saved.update({'phase':'promoted','promotion':result})
    report.write_text(json.dumps(saved,indent=2))
