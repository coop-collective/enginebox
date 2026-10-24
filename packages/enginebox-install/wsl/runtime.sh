#!/bin/bash
set -euo pipefail
umask 077
export PATH=/usr/local/bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin
export DOCKER_HOST=unix:///run/enginebox-docker.sock
[[ $(id -u) == 0 && -f /etc/enginebox/wsl-runtime ]] || { echo 'Not an owned Enginebox runtime' >&2; exit 1; }
case "${1:-}" in
  claim)
    read -r owner
    [[ "$owner" =~ ^S-1-[0-9-]+$ && ! -e /etc/enginebox/installation-owner ]] || exit 1
    printf '%s\n' "$owner" > /etc/enginebox/installation-owner
    ;;
  configure)
    [[ $(uname -m) == x86_64 ]] || { echo 'Linux x64 is required' >&2; exit 1; }
    [[ $(findmnt -n -o FSTYPE /var/lib/enginebox) == ext4 ]] || { echo 'WSL ext4 filesystem required' >&2; exit 1; }
    [[ $(stat -fc %T /sys/fs/cgroup) == cgroup2fs ]] || { echo 'WSL2 with cgroup v2 is required' >&2; exit 1; }
    incoming=$(mktemp /etc/enginebox/settings.XXXXXX)
    trap 'rm -f "$incoming"' EXIT
    cat > "$incoming"
    python3 - "$incoming" <<'PY'
import json,pathlib,sys,urllib.parse
p=pathlib.Path(sys.argv[1]); s=json.loads(p.read_text(encoding='utf-8-sig'))
assert set(s)=={'controller','cpu','memory_gib','disk_gib'}
u=urllib.parse.urlsplit(s['controller']); assert u.scheme=='https' and u.hostname and not u.username and not u.password and not u.path and not u.query and not u.fragment
for key,lo,hi in [('cpu',1,256),('memory_gib',4,4096),('disk_gib',20,65536)]: assert type(s[key]) is int and lo<=s[key]<=hi
saved=pathlib.Path('/etc/enginebox/settings.json')
if saved.exists(): assert json.loads(saved.read_text())==s, 'Existing runtime settings differ'
else: saved.write_text(json.dumps(s))
pathlib.Path('/etc/systemd/system/enginebox.slice').write_text('[Unit]\nDescription=Enginebox contribution budget\n[Slice]\nCPUQuota='+str(s['cpu']*100)+'%\nMemoryMax='+str(s['memory_gib'])+'G\nMemorySwapMax=0\nTasksMax=4096\n')
PY
    systemctl daemon-reload
    systemctl start enginebox-docker.service
    for attempt in $(seq 1 30); do docker info >/dev/null 2>&1 && break; sleep 1; done
    if [[ ! -f /etc/enginebox/layerfs.json ]]; then
      docker load --input /opt/enginebox/template.tar
      image=$(docker image ls --no-trunc --format '{{.ID}}' 'enginebox-wsl-template:*' | sort -u)
      [[ "$image" =~ ^sha256:[a-f0-9]{64}$ ]] || { echo 'Expected one immutable template image' >&2; exit 1; }
      python3 - "$image" <<'PY'
import hashlib,json,pathlib,sys
s=json.loads(pathlib.Path('/etc/enginebox/settings.json').read_text())
hashfile=lambda p:hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
config={'schema':1,'root':'/var/lib/enginebox/layerfs','docker_socket':'/run/enginebox-docker.sock','sdk':'/opt/enginebox/layerfs/sdk','daemon':'/opt/enginebox/layerfs/daemon','native_sha256':hashfile('/opt/enginebox/layerfs/sdk/native/layerfs.node'),'daemon_sha256':hashfile('/opt/enginebox/layerfs/daemon'),'image':sys.argv[1],'max_workspaces':4,'cpus_per_workspace':min(s['cpu'],64),'memory_mib_per_workspace':min((s['memory_gib']-2)*1024,131072),'disk_reserve_gib':2,'import_limit_gib':1}
pathlib.Path('/etc/enginebox/layerfs.json').write_text(json.dumps(config))
PY
    fi
    # Qualification runs under the same aggregate cgroup as actual workloads.
    systemd-run --quiet --wait --pipe --collect --slice=enginebox.slice /usr/bin/python3 /opt/enginebox/qualify.py
    ;;
  enrolled)
    test -f /var/lib/enginebox/private/worker.json
    ;;
  enroll)
    if [[ -f /var/lib/enginebox/private/worker.json ]] && jq -e '.enrolled == true' /var/lib/enginebox/private/worker.json >/dev/null; then
      echo 'This runtime is already registered.'
      exit 0
    fi
    controller=$(jq -r .controller /etc/enginebox/settings.json)
    cpu=$(jq -r .cpu /etc/enginebox/settings.json)
    memory=$(jq -r .memory_gib /etc/enginebox/settings.json)
    disk=$(jq -r .disk_gib /etc/enginebox/settings.json)
    exec enginebox-node register-worker --controller "$controller" --cpu "$cpu" --memory-gib "$memory" --disk-gib "$disk" --config /var/lib/enginebox/private/worker.json
    ;;
  supervise)
    systemctl start enginebox-worker.service
    while systemctl is-active --quiet enginebox-worker.service; do sleep 5; done
    echo 'Worker stopped. Check dashboard status and the private WSL service journal.' >&2
    exit 1
    ;;
  *) echo 'Expected claim, configure, enrolled, enroll or supervise' >&2; exit 2 ;;
esac
