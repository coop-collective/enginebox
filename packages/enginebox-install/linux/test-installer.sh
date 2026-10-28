#!/usr/bin/env bash
# This test writes fake service files only inside its own disposable container.
set -euo pipefail
[[ ${ENGINEBOX_INSTALLER_TEST_CONTAINER:-} == 1 && -f /.dockerenv && $(id -u) == 0 ]] || {
  echo 'Run only in the disposable installer test container.' >&2; exit 1;
}
source_dir=$(cd -- "$(dirname -- "$0")" && pwd)
fixture=$(mktemp -d /opt/enginebox-installer-test.XXXXXXXX)
trap 'rm -rf -- "$fixture"' EXIT
mkdir -p "$fixture/tools" /etc/systemd/system
export FIXTURE_ROOT="$fixture"
export PATH="$fixture/tools:$PATH"
cat > "$fixture/node" <<'PY'
#!/usr/bin/env python3
import json,os,pathlib,sys
if sys.argv[1:]==['--version']:
 print('enginebox-node fixture'); raise SystemExit(0)
assert sys.argv[1]=='register-host'
args=dict(zip(sys.argv[2::2],sys.argv[3::2]))
code=sys.stdin.readline().strip(); assert '.' in code
p=pathlib.Path(args['--config'])
assert not p.exists(), 'Fixture must not be re-enrolled'
p.write_text(json.dumps({'schema':1,'controller':args['--controller'],'id':code.split('.')[0],'token':'fixture-only','enrolled':True,'budget':{'cpu':4,'memory_gib':8,'disk_gib':100},'binding':{'host_id':'fixture'}}))
p.chmod(0o600)
with open(pathlib.Path(os.environ['FIXTURE_ROOT'])/'enrollments','a') as f:f.write('registered\n')
PY
cat > "$fixture/tools/curl" <<'PY'
#!/usr/bin/env python3
import os,pathlib,shutil,sys
assert '--proto' in sys.argv and '--proto-redir' in sys.argv
assert any(a.startswith('https://github.com/coop-collective/enginebox/releases/download/') for a in sys.argv)
shutil.copyfile(pathlib.Path(os.environ['FIXTURE_ROOT'])/'node',sys.argv[sys.argv.index('--output')+1])
PY
cat > "$fixture/tools/systemctl" <<'SH'
#!/usr/bin/env bash
set -eu
printf '%s\n' "$*" >> "$FIXTURE_ROOT/systemctl.log"
if [[ $1 == restart ]]; then
python3 - <<'PY'
import json,pathlib,re,time
unit=pathlib.Path('/etc/systemd/system/enginebox-managed-node.service').read_text()
config=pathlib.Path(re.search(r'^ExecStart=.* --config (.+)$',unit,re.M)[1]); c=json.loads(config.read_text())
(config.parent/'status.json').write_text(json.dumps({'id':c['id'],'online':True,'ready':True,'blocked':False,'last_seen':int(time.time())}))
PY
fi
SH
cat > "$fixture/tools/enginebox-host" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod 0755 "$fixture/node" "$fixture/tools/"*
python3 "$source_dir/package.py" --binary "$fixture/node" --output "$fixture/package" --tag linux-node-preview-1 --commit 1111111111111111111111111111111111111111
code='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'
prefix="$fixture/installed"
installer="$fixture/package/install-linux-node.sh"
bash "$installer" --controller https://controller.example --enrollment-code "$code" --prefix "$prefix"
before=$(sha256sum "$prefix/private/host.json")
bash "$installer" --controller https://controller.example --enrollment-code "$code" --prefix "$prefix"
[[ "$before" == "$(sha256sum "$prefix/private/host.json")" ]]
[[ $(wc -l < "$fixture/enrollments") == 1 ]]
python3 - "$prefix" "$fixture" <<'PY'
import pathlib,sys
p=pathlib.Path(sys.argv[1]); fixture=pathlib.Path(sys.argv[2])
assert p.stat().st_mode&0o077==0
assert (p/'private/host.json').stat().st_mode&0o077==0
unit=pathlib.Path('/etc/systemd/system/enginebox-managed-node.service').read_text()
for expected in ['Restart=always','RestartSec=15','WantedBy=multi-user.target','NoNewPrivileges=yes','ProtectSystem=strict']:
 assert expected in unit
assert f'ExecStart={p}/bin/enginebox-node run-host --config {p}/private/host.json' in unit
assert 'enable enginebox-managed-node.service' in (fixture/'systemctl.log').read_text()
assert not any(p.glob('private/download.*'))
PY
# A copied command for another machine/controller must preserve this identity.
if bash "$installer" --controller https://other.example --enrollment-code "$code" --prefix "$prefix" >/dev/null 2>&1; then exit 1; fi
[[ "$before" == "$(sha256sum "$prefix/private/host.json")" ]]
mkdir "$fixture/unowned"
echo retain > "$fixture/unowned/owner-file"
if bash "$installer" --controller https://controller.example --enrollment-code "$code" --prefix "$fixture/unowned" >/dev/null 2>&1; then exit 1; fi
[[ $(cat "$fixture/unowned/owner-file") == retain ]]
# Reject a substituted executable before replacing the installed one.
installed=$(sha256sum "$prefix/bin/enginebox-node")
printf '\n# modified\n' >> "$fixture/node"
if bash "$installer" --controller https://controller.example --enrollment-code "$code" --prefix "$prefix" >/dev/null 2>&1; then exit 1; fi
[[ "$installed" == "$(sha256sum "$prefix/bin/enginebox-node")" ]]
# Never take over another service under the same name.
echo '# unrelated service' > /etc/systemd/system/enginebox-managed-node.service
if bash "$installer" --controller https://controller.example --enrollment-code "$code" --prefix "$prefix" >/dev/null 2>&1; then exit 1; fi
echo 'Linux installer: identity preservation, boot service, ownership and checksum rejection passed.'
