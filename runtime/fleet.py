"""Capacity-aware Incus fleet operations, with a bounded asynchronous job queue."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import select
import shlex
import subprocess
import threading
import time
import tomllib
import uuid
from urllib.parse import quote

from enginebox import EngineBox, GuestConfig, identifier
from templates import Registry, host_binding_digest, validate_resources
from warm_pool import WarmPool

GIB = 1024**3
SIZES = {"small": {"cpu": 2, "memory_gib": 2},
         "standard": {"cpu": 4, "memory_gib": 4},
         "large": {"cpu": 8, "memory_gib": 8}}
ACTIVE = {"Running", "Frozen", "Starting", "Stopping"}


def positive(value, name, *, minimum=0, maximum=4096):
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ValueError(f"{name} must be an integer between {minimum} and {maximum}.")
    return value


@dataclass(frozen=True)
class Host:
    id: str
    label: str
    target: str
    project: str
    pool: str
    reserve_cpu: int
    reserve_memory_gib: int
    reserve_disk_gib: int
    disk_gib: int
    max_instances: int
    templates: tuple[dict, ...]
    guest: GuestConfig = GuestConfig(1001, 1001, "agent", "/home/agent", "/workspace", ":99")
    network: str | None = None
    warm_pools: tuple[dict, ...] = ()

    def box(self):
        return EngineBox(self.target, project=self.project, guest=self.guest)


def load_hosts(path: str | Path) -> list[Host]:
    with Path(path).open("rb") as stream:
        config = tomllib.load(stream)
    if type(config.get("schema_version")) is not int or config.get("schema_version") != 1 or not isinstance(config.get("hosts"), list) or not config["hosts"]:
        raise ValueError("Fleet config requires schema_version = 1 and at least one [[hosts]].")
    if set(config) != {"schema_version", "hosts"}:
        raise ValueError("Unknown fleet configuration sections.")
    hosts, ids, targets = [], set(), set()
    for entry in config["hosts"]:
        allowed = {"id", "label", "target", "project", "pool", "reserve_cpu", "reserve_memory_gib", "reserve_disk_gib", "disk_gib", "max_instances", "templates", "guest", "network", "warm_pools"}
        if not isinstance(entry, dict) or set(entry) - allowed or not {"id", "target"} <= set(entry):
            raise ValueError("Each host requires id/target and only supported host fields.")
        for key in ["id", "target", "label", "project", "pool", "network"]:
            if key in entry and not isinstance(entry[key], str):
                raise ValueError(f"hosts.{key} must be a string.")
        host_id = identifier(entry["id"])
        target = entry["target"]
        project = entry.get("project", "enginebox")
        guest = entry.get("guest", {})
        if not isinstance(guest, dict) or set(guest) - {"uid", "gid", "user", "home", "workspace", "display"}:
            raise ValueError("hosts.guest accepts only uid, gid, user, home, workspace and display.")
        for key in ["uid", "gid"]:
            if key in guest:
                positive(guest[key], f"guest.{key}", minimum=1, maximum=4294967294)
        for key in ["user", "home", "workspace", "display"]:
            if key in guest and not isinstance(guest[key], str):
                raise ValueError(f"guest.{key} must be a string.")
        guest_config = GuestConfig.from_mapping({"ENGINEBOX_GUEST_" + key.upper(): str(value) for key, value in guest.items()})
        EngineBox(target, project=project, guest=guest_config)  # independent of operator environment defaults
        if host_id in ids or target in targets:
            raise ValueError("Host IDs and SSH targets must be unique; configure a machine only once.")
        ids.add(host_id)
        targets.add(target)
        pool = identifier(entry.get("pool", "enginebox"))
        network = identifier(entry["network"]) if "network" in entry else None
        templates, template_ids = [], set()
        if not isinstance(entry.get("templates", []), list):
            raise ValueError("hosts.templates must be an array of template tables.")
        for template in entry.get("templates", []):
            if (not isinstance(template, dict) or set(template) - {"id", "label", "source", "snapshot", "minimum", "recommended"}
                    or not {"id", "source", "snapshot"} <= set(template)
                    or not all(isinstance(v, str) for k, v in template.items() if k not in {"minimum", "recommended"})):
                raise ValueError("Templates require string id/source/snapshot and optional label.")
            for field in ("id", "snapshot"):
                identifier(template[field])
            identifier(template["source"], instance=True)
            if template["id"] in template_ids:
                raise ValueError("Template IDs must be unique on a host.")
            template_ids.add(template["id"])
            templates.append({"id": template["id"], "label": template.get("label", template["id"]),
                              "source": template["source"], "snapshot": template["snapshot"]})
            if "minimum" in template or "recommended" in template:
                if not {"minimum", "recommended"} <= set(template):
                    raise ValueError("Template minimum and recommended resources must be supplied together.")
                templates[-1]["minimum"] = validate_resources(template["minimum"])
                templates[-1]["recommended"] = validate_resources(template["recommended"], template["minimum"])
        policies, policy_ids = [], set()
        if not isinstance(entry.get("warm_pools", []), list):
            raise ValueError("hosts.warm_pools must be an array of pool tables.")
        for policy in entry.get("warm_pools", []):
            if not isinstance(policy, dict) or set(policy) != {"template", "size", "ready"}:
                raise ValueError("Warm pools require template, size and ready fields.")
            if not isinstance(policy["template"], str) or not isinstance(policy["size"], str):
                raise ValueError("Warm pool template and size must be strings.")
            identifier(policy["template"])
            if policy["size"] not in {*SIZES, "minimum", "recommended"}:
                raise ValueError("Choose a minimum/recommended or legacy size for a warm pool.")
            positive(policy["ready"], "warm_pools.ready", maximum=32)
            key = (policy["template"], policy["size"])
            if key in policy_ids:
                raise ValueError("Warm pool template/size pairs must be unique.")
            policy_ids.add(key)
            policies.append(dict(policy))
        hosts.append(Host(host_id, entry.get("label", host_id), target, project, pool,
                          positive(entry.get("reserve_cpu", 2), "reserve_cpu"),
                          positive(entry.get("reserve_memory_gib", 4), "reserve_memory_gib"),
                          positive(entry.get("reserve_disk_gib", 10), "reserve_disk_gib"),
                          positive(entry.get("disk_gib", 30), "disk_gib", minimum=8),
                          positive(entry.get("max_instances", 16), "max_instances", minimum=1, maximum=256),
                          tuple(templates), guest_config, network, tuple(policies)))
    return hosts


# Inventory is reduced on the host: no image/user configuration or hardware
# serial numbers leave it. Querying metadata avoids asking a frozen guest agent.
INVENTORY_SCRIPT = r'''
import json, os, re, subprocess, sys
from urllib.parse import quote
pool, project = sys.argv[1:3]
def query(path):
    return json.loads(subprocess.check_output(['incus', 'query', path], timeout=20))
def size(value, fallback):
    match=re.fullmatch(r'([0-9]+)([KMGT]i?B)?', str(value))
    if not match: return fallback
    unit=match[2] or ''
    return int(match[1]) * {'':1,'KB':1000,'MB':1000**2,'GB':1000**3,'TB':1000**4,
                           'KiB':1024,'MiB':1024**2,'GiB':1024**3,'TiB':1024**4}[unit]
def cpus(value):
    if str(value).isdigit(): return int(value) or os.cpu_count()
    if not value: return os.cpu_count()
    result=set()
    try:
        for part in value.split(','):
            lo,*hi=part.split('-')
            low,high=int(lo),int(hi[0] if hi else lo)
            if low<0 or high<low or high-low>4096: return os.cpu_count()
            result.update(range(low,high+1))
        return len(result) or os.cpu_count()
    except ValueError: return os.cpu_count()
mem={}
for line in open('/proc/meminfo'):
    key,value=line.split(':',1); mem[key]=int(value.split()[0])*1024
# recursion=2 requests live guest state and can stall on frozen VMs. Metadata
# plus bridge leases gives admission a complete view without contacting guests.
data=query('/1.0/instances?recursion=1&all-projects=true')
leases={}
def addresses(vm):
    found=[]
    cfg=vm.get('expanded_config', vm.get('config',{}))
    vm_project=vm.get('project','default')
    for name, device in vm.get('expanded_devices',{}).items():
        network=device.get('network') or (device.get('parent') if device.get('nictype')=='bridged' else None)
        if device.get('type')!='nic' or not network: continue
        # Even a globally shared bridge filters leases by the requesting project.
        # Keep this context in both the query and cache key; querying default
        # would omit VMs in other projects, including a features.networks=false
        # project that uses the default project's managed bridge.
        lease_key=(vm_project,network)
        if lease_key not in leases:
            try: leases[lease_key]=query('/1.0/networks/'+quote(network,safe='')+'/leases?project='+quote(vm_project,safe=''))
            except (subprocess.SubprocessError, ValueError): leases[lease_key]=[]
        mac=device.get('hwaddr') or cfg.get('volatile.'+name+'.hwaddr')
        if not mac: continue
        for lease in leases[lease_key]:
            address=lease.get('address','')
            if str(lease.get('hwaddr','')).lower()==mac.lower() and ':' not in address and address: found.append(address)
    return sorted(set(found))
def pool_metadata(vm, cfg):
    state=cfg.get('user.enginebox.pool.state')
    claim=None
    description=vm.get('description','')
    if description.startswith('enginebox-pool:v1:'):
        try:
            claim=json.loads(description[len('enginebox-pool:v1:'):])
            if not isinstance(claim,dict) or claim.get('state') not in {'claimed','quarantined'}: raise ValueError()
            if claim['state']=='claimed' and not re.fullmatch(r'ebox-[a-z0-9][a-z0-9-]{0,48}',str(claim.get('workspace_name',''))): raise ValueError()
            state=claim['state']
        except (ValueError,TypeError):
            state='quarantined'; claim={'state':state,'reason':'Invalid pool reservation; inspect before reuse.'}
    # A claim in either durable location permanently excludes this VM from reuse.
    if cfg.get('user.enginebox.workspace') and not claim:
        state='claimed'; claim={'state':state,'workspace_name':cfg['user.enginebox.workspace']}
    return {'warm_pool_state':state,'pool_claim':claim,
            'workspace_name':(claim or {}).get('workspace_name') or cfg.get('user.enginebox.workspace'),
            'pool_generation':cfg.get('user.enginebox.pool.generation'),
            'pool_size':cfg.get('user.enginebox.pool.size'),
            'pool_created_at':cfg.get('user.enginebox.pool.created_at')}
items=[]
for vm in data:
    cfg=vm.get('expanded_config', vm.get('config',{}))
    root=next((v for v in vm.get('expanded_devices',{}).values() if v.get('path')=='/' and v.get('type')=='disk'),{})
    state=vm.get('state') or {}
    items.append({'name':vm['name'],'project':vm.get('project','default'),'status':vm['status'],
      'type':vm.get('type'),'stateful':vm.get('stateful',False),
      'cpu':cpus(cfg.get('limits.cpu')),'memory_bytes':size(cfg.get('limits.memory'),mem['MemTotal']),
      'memory_usage_bytes':None,
      'pool':root.get('pool'),'disk_bytes':size(root.get('size'),30*1024**3),
      'state_disk_bytes':size(root.get('size.state'),0),
      'managed':cfg.get('user.enginebox.fleet')=='true',
      'template':cfg.get('user.enginebox.template'),
      'ipv4':addresses(vm),**pool_metadata(vm,cfg)})
space=query('/1.0/storage-pools/'+quote(pool,safe='')+'/resources')['space']
fs=os.statvfs('/var/lib/incus')
print(json.dumps({'cpu_total':os.cpu_count(),'memory_total_bytes':mem['MemTotal'],
 'memory_available_bytes':mem['MemAvailable'],'disk_free_bytes':fs.f_bavail*fs.f_frsize,
 'pool_total_bytes':space['total'],'pool_used_bytes':space['used'],'instances':items}))
'''


def host_run(host: Host, argv: list[str], *, timeout=120, input_data: bytes | None = None) -> bytes:
    command = ["sudo", "-n", *argv]
    if host.target != "local":
        command = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host.target, shlex.join(command)]
    result = subprocess.run(command, input=input_data, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace").strip() or "Host command failed.")
    return result.stdout


@contextmanager
def host_operation_lock(host: Host):
    """Hold a root-owned host flock across the operation's separate SSH calls."""
    script = """import fcntl,os,sys
fd=os.open('/run/lock/enginebox-host.lock',os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600)
if os.fstat(fd).st_uid != 0 or os.fstat(fd).st_mode & 0o022:
    raise SystemExit('Host lock must be root-owned and not group/world writable.')
try: fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
except BlockingIOError: raise SystemExit('Another Engine Box operation holds the host lock.')
print('locked',flush=True)
sys.stdin.buffer.read()
"""
    command = ["sudo", "-n", "python3", "-u", "-c", script]
    if host.target != "local":
        command = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10",
                   "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3", host.target, shlex.join(command)]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        if not select.select([process.stdout], [], [], 30)[0] or process.stdout.readline() != b"locked\n":
            raise RuntimeError("Cannot acquire host operation lock; another installer/controller may be active.")
        yield
    finally:
        process.stdin.close()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        process.stdout.close()
        process.stderr.close()


def inventory(host: Host) -> dict:
    return json.loads(host_run(host, ["python3", "-c", INVENTORY_SCRIPT, host.pool, host.project], timeout=60))


def capacity(host: Host, facts: dict) -> dict:
    # Unknown/transitional states may still own RAM/vCPUs. Only a confirmed
    # Stopped instance releases these budgets.
    active = [item for item in facts["instances"] if item["status"] != "Stopped"]
    allocated_cpu = sum(item["cpu"] for item in active)
    allocated_memory = sum(item["memory_bytes"] for item in active)
    # Full disk commitments are counted even for stopped clones. This is
    # deliberately conservative on copy-on-write pools and prevents thin-pool
    # overcommit from becoming the default fleet policy.
    disk_committed = sum(item["disk_bytes"] + item["state_disk_bytes"]
                         for item in facts["instances"] if item.get("pool") == host.pool)
    cpu_room = max(0, facts["cpu_total"] - host.reserve_cpu - allocated_cpu)
    memory_room = max(0, min(facts["memory_total_bytes"] - host.reserve_memory_gib * GIB - allocated_memory,
                             facts["memory_available_bytes"] - host.reserve_memory_gib * GIB))
    disk_room = max(0, min(facts["disk_free_bytes"],
                           facts["pool_total_bytes"] - facts["pool_used_bytes"],
                           facts["pool_total_bytes"] - disk_committed) - host.reserve_disk_gib * GIB)
    count = sum(item.get("managed", False) and item["project"] == host.project for item in facts["instances"])
    slots = max(0, host.max_instances - count)
    additional = {name: max(0, min(slots, cpu_room // size["cpu"],
                       memory_room // (size["memory_gib"] * GIB),
                       disk_room // ((host.disk_gib + size["memory_gib"] + 2) * GIB)))
                  for name, size in SIZES.items()}
    return {key: facts[key] for key in ("cpu_total", "memory_total_bytes", "memory_available_bytes", "disk_free_bytes")} | {
        "pool_total_bytes": facts["pool_total_bytes"], "pool_free_bytes": facts["pool_total_bytes"] - facts["pool_used_bytes"],
        "reserve_cpu": host.reserve_cpu, "reserve_memory_bytes": host.reserve_memory_gib * GIB,
        "allocated_cpu": allocated_cpu, "allocated_memory_bytes": allocated_memory,
        "disk_committed_bytes": disk_committed, "max_additional": additional,
        "cpu_available": cpu_room, "memory_budget_available_bytes": memory_room,
        "disk_budget_available_bytes": disk_room, "instance_slots_available": slots}


def resource_bytes(value: str | int, fallback: int) -> int:
    match = re.fullmatch(r"([0-9]+)([KMGT]i?B)?", str(value))
    if not match:
        return fallback
    factors = {"": 1, "KB": 1000, "MB": 1000**2, "GB": 1000**3, "TB": 1000**4,
               "KiB": 1024, "MiB": 1024**2, "GiB": 1024**3, "TiB": 1024**4}
    return int(match[1]) * factors[match[2] or ""]


def resource_fits(host: Host, facts: dict, resources: dict) -> bool:
    room = capacity(host, facts)
    return (room["instance_slots_available"] >= 1 and room["cpu_available"] >= resources["cpu"]
            and room["memory_budget_available_bytes"] >= resources["memory_gib"] * GIB
            and room["disk_budget_available_bytes"] >= (resources["disk_gib"] + resources["memory_gib"] + 2) * GIB)


def root_device(box, instance: dict, project: str) -> tuple[str, dict]:
    devices = instance.get("expanded_devices")
    if devices is None:
        devices = {}
        for profile in instance.get("profiles", []):
            profile_data = json.loads(box.run("query", f"/1.0/profiles/{quote(profile, safe='')}?project={quote(project, safe='')}"))
            devices.update(profile_data.get("devices", {}))
        devices.update(instance.get("devices", {}))
    roots = [(name, device) for name, device in devices.items()
             if device.get("type") == "disk" and device.get("path") == "/"]
    if len(roots) != 1:
        raise ValueError("Template/instance must resolve exactly one root disk.")
    return roots[0]


READINESS_SCRIPT = r'''
import ipaddress, json, os, pathlib, pwd, shlex, shutil, socket, subprocess, sys
expected=json.loads(sys.argv[1])
account=pwd.getpwuid(os.getuid())
if (os.getuid()!=expected['uid'] or os.getgid()!=expected['gid'] or account.pw_name!=expected['user']
        or account.pw_dir!=expected['home'] or os.path.realpath(os.getcwd())!=os.path.realpath(expected['workspace'])):
    raise SystemExit('Guest identity/home/workspace does not match the fleet configuration')
if subprocess.run(['systemctl','is-active','--quiet','enginebox-identity.service'],timeout=5).returncode:
    raise SystemExit('Clone identity service is not active')
instance_id=pathlib.Path('/var/lib/enginebox/instance-uuid').read_text().strip()
machine_id=pathlib.Path('/etc/machine-id').read_text().strip()
if not instance_id or instance_id!=machine_id:
    raise SystemExit('Clone machine identity does not match its initialized VM identity')
pid1=shlex.split(subprocess.check_output(['busctl','call','org.freedesktop.systemd1','/org/freedesktop/systemd1',
                                        'org.freedesktop.DBus.Peer','GetMachineId'],text=True,timeout=5))
if len(pid1)!=2 or pid1[0]!='s' or pid1[1]!=machine_id:
    raise SystemExit('PID 1 retains a stale machine identity; rebuild the prepared template')
if not pathlib.Path('/etc/cloud/cloud-init.disabled').is_file():
    raise SystemExit('Prepared image has not disabled completed cloud-init setup')
manifest=json.loads(pathlib.Path('/opt/enginebox-runtime/installed-versions.json').read_text())
if manifest.get('user')!=expected['user'] or not isinstance(manifest.get('clients'),dict):
    raise SystemExit('Preparation manifest does not match the configured guest')
clients=[]
for command,details in manifest['clients'].items():
    if command not in ('codex','claude','agent','node','npm'): continue
    if not isinstance(details,dict) or details.get('returncode')!=0 or not details.get('version') or not shutil.which(command):
        raise SystemExit('Prepared client is unavailable: '+command)
    if command in ('codex','claude','agent'): clients.append(command)
browser=bool(manifest.get('playwright_mcp'))
if browser:
    package=pathlib.Path('/opt/enginebox-browser/current')
    chromium=pathlib.Path((package/'chromium-path.txt').read_text().strip())
    if not chromium.is_absolute() or not os.access(chromium,os.X_OK):
        raise SystemExit('Prepared browser executable is unavailable')
interfaces=json.loads(subprocess.check_output(['ip','-j','-4','address','show'],timeout=5))
addresses=[item['local'] for link in interfaces for item in link.get('addr_info',[])
           if item.get('scope')=='global' and not ipaddress.ip_address(item['local']).is_loopback]
routes=json.loads(subprocess.check_output(['ip','-j','-4','route','show','default'],timeout=5))
if not addresses or not routes: raise SystemExit('Guest IPv4/default route is not ready')
socket.getaddrinfo('example.com',443,socket.AF_INET,socket.SOCK_STREAM)
print(json.dumps({'ready':True,'ipv4':addresses,'native_clients':sorted(clients),'browser':browser,'dns':True,'identity':True}))
'''


class Fleet:
    def __init__(self, hosts: list[Host], *, state_dir: Path | None = None, registry=None):
        if not hosts or len({host.id for host in hosts}) != len(hosts) or len({host.target for host in hosts}) != len(hosts):
            raise ValueError("Fleet needs at least one host with unique IDs and targets.")
        self.hosts = {host.id: host for host in hosts}
        self.locks = {host.id: threading.Lock() for host in hosts}
        self.executor = ThreadPoolExecutor(max_workers=4, thread_name_prefix="enginebox-fleet")
        self.pending = threading.BoundedSemaphore(32)
        self.jobs_lock = threading.Lock()
        self.jobs: dict[str, dict] = {}
        self.state_dir = state_dir.expanduser().resolve() if state_dir else None
        self.registry = registry if registry is not None else (Registry(self.state_dir.parent) if self.state_dir else None)
        self.warm = WarmPool(self)
        if state_dir:
            state_dir = self.state_dir
            state_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
            if state_dir.stat().st_uid != os.geteuid() or state_dir.stat().st_mode & 0o077:
                raise ValueError("Fleet state directory must be owned by the service user with mode 0700.")
            recovered = []
            for path in sorted(state_dir.glob("*.json"), key=lambda item: item.stat().st_mtime)[-200:]:
                if path.is_symlink():
                    raise ValueError("Fleet job records must not be symbolic links.")
                job = json.loads(path.read_text())
                if (not isinstance(job, dict) or not re.fullmatch(r"[a-f0-9]{32}", str(job.get("id", "")))
                        or path.name != job["id"] + ".json" or job.get("status") not in {"queued", "running", "failed", "succeeded"}):
                    raise ValueError(f"Invalid fleet job record: {path.name}")
                if job["status"] in {"queued", "running"}:
                    job.update(status="failed", error="Service restarted; inspect the VM before retrying.")
                    recovered.append(job)
                self.jobs[job["id"]] = job
            for job in recovered:
                self._save(job)

    def host(self, host_id):
        if host_id not in self.hosts:
            raise ValueError("Unknown host.")
        return self.hosts[host_id]

    def _host_status(self, host):
        try:
            facts = inventory(host)
            return {"id": host.id, "label": host.label, "online": True, "capacity": capacity(host, facts),
                    "warm_pools": self.warm.summary(host, facts),
                    "instances": [item for item in facts["instances"]
                                  if item["project"] == host.project and item["name"].startswith("ebox-")]}
        except (OSError, RuntimeError, ValueError, KeyError, subprocess.TimeoutExpired):
            # Host aliases and raw command diagnostics stay out of the browser.
            return {"id": host.id, "label": host.label, "online": False,
                    "error": "Host inventory unavailable. Check SSH, Incus access and storage configuration.", "instances": []}

    def summary(self):
        with ThreadPoolExecutor(max_workers=min(8, len(self.hosts))) as pool:
            hosts = list(pool.map(self._host_status, self.hosts.values()))
        templates = {}
        for host in self.hosts.values():
            for item in self.templates_for(host):
                entry = templates.setdefault(item["id"], {key: item[key] for key in ("id", "label", "minimum", "recommended")} | {"host_ids": [], "host_resources": {}})
                entry["host_ids"].append(host.id)
                entry["host_resources"][host.id] = {key: item[key] for key in ("minimum", "recommended")}
        return {"hosts": hosts, "templates": list(templates.values()), "sizes": SIZES}

    def catalog(self):
        records = self.registry.list() if self.registry else []
        for record in records:
            for deployment in record["deployments"]:
                host = self.hosts.get(deployment["host_id"])
                deployment["in_sync"] = bool(deployment["in_sync"] and host
                    and deployment["host_binding_digest"] == host_binding_digest(host))
        return {"templates": records}

    def templates_for(self, host):
        # Static bases remain supported for existing installations. Registering
        # the same ID transfers its resource/version contract to the registry.
        records = {item["id"]: item for item in self.registry.list()} if self.registry else {}
        result = []
        for item in host.templates:
            if item["id"] in records:
                continue
            result.append(dict(item) | {
                "minimum": item.get("minimum", SIZES["small"] | {"disk_gib": host.disk_gib}),
                "recommended": item.get("recommended", SIZES["standard"] | {"disk_gib": host.disk_gib})})
        if self.registry:
            binding = host_binding_digest(host)
            for installed in self.registry.deployments(host.id):
                record = records.get(installed["id"])
                if (record and installed["manifest_digest"] == record["manifest_digest"]
                        and installed["host_binding_digest"] == binding):
                    result.append({key: installed[key] for key in ("id", "label", "source", "snapshot", "minimum", "recommended",
                                                                  "revision", "manifest_digest")})
        return result

    def template(self, host, template_id):
        result = next((t for t in self.templates_for(host) if t["id"] == template_id), None)
        if result is None:
            raise ValueError("Select a template installed at the current revision on this host.")
        return result

    @staticmethod
    def resources(host, template, size=None, resources=None):
        if resources is not None:
            if size is not None:
                raise ValueError("Choose a size preset or custom resources, not both.")
            return validate_resources(resources, template["minimum"])
        size = size or "recommended"
        if size in {"minimum", "recommended"}:
            return validate_resources(template[size], template["minimum"])
        if size in SIZES:
            return validate_resources(SIZES[size] | {"disk_gib": host.disk_gib}, template["minimum"])
        raise ValueError("Choose minimum, recommended, or custom resources.")

    @staticmethod
    def template_snapshot(host, source):
        snapshot = json.loads(host.box().run("query", f"/1.0/instances/{source['source']}/snapshots/{source['snapshot']}?project={quote(host.project, safe='')}"))
        if snapshot.get("config", {}).get("user.enginebox.clean-base") != "true":
            raise ValueError("Template must be prepared before login and marked user.enginebox.clean-base=true at snapshot time.")
        if snapshot.get("stateful"):
            raise ValueError("Reusable templates must be disk-only snapshots.")
        if snapshot.get("type", "virtual-machine") != "virtual-machine":
            raise ValueError("Reusable fleet templates must be virtual machines.")
        return snapshot

    @staticmethod
    def check_name(host, name, facts):
        identifier(name, instance=True)
        if any(vm["project"] == host.project and name in {vm["name"], vm.get("workspace_name")} for vm in facts["instances"]):
            raise ValueError("Instance or workspace name already exists; choose a new name.")

    def _save(self, job):
        if self.state_dir:
            temporary = self.state_dir / (job["id"] + ".tmp")
            descriptor = os.open(temporary, os.O_CREAT | os.O_TRUNC | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
            with os.fdopen(descriptor, "w") as stream:
                json.dump(job, stream, indent=2)
                stream.write("\n")
                stream.flush()
                os.fsync(stream.fileno())
            temporary.replace(self.state_dir / (job["id"] + ".json"))
        completed = sorted((value for value in self.jobs.values() if value["status"] in {"failed", "succeeded"}),
                           key=lambda value: value.get("created_at", ""))
        for old in completed[:max(0, len(self.jobs) - 200)]:
            self.jobs.pop(old["id"], None)
            if self.state_dir:
                (self.state_dir / (old["id"] + ".json")).unlink(missing_ok=True)

    def job(self, job_id):
        with self.jobs_lock:
            if job_id not in self.jobs:
                raise ValueError("Unknown job.")
            return json.loads(json.dumps(self.jobs[job_id]))

    def submit(self, host_id, operation, **params):
        host = self.host(host_id)
        if operation not in {"create", "allocate", "install_template", "pause", "resume", "hibernate", "stop"}:
            raise ValueError("Unsupported fleet operation.")
        if operation in {"create", "allocate"}:
            if (not {"name", "template"} <= set(params) or set(params) - {"name", "template", "size", "resources"}
                    or ("size" in params and "resources" in params)
                    or not isinstance(params["name"], str) or not isinstance(params["template"], str)
                    or ("size" in params and not isinstance(params["size"], str))):
                raise ValueError("Provide name, template and either a size preset or custom resources.")
            if "resources" in params and not isinstance(params["resources"], dict):
                raise ValueError("Custom resources must be an object.")
            identifier(params["name"], instance=True)
            self.resources(host, self.template(host, params["template"]), params.get("size"), params.get("resources"))
            name = params["name"]
        elif operation == "install_template":
            if set(params) != {"template"} or not isinstance(params["template"], str) or not self.registry:
                raise ValueError("Provide a registered template ID; configure a persistent registry first.")
            self.registry.get(params["template"])
            name = params["template"]
        else:
            if set(params) != {"name"} or not isinstance(params["name"], str):
                raise ValueError("Provide only the required string field name.")
            identifier(params["name"], instance=True)
            name = params["name"]
        if not self.pending.acquire(blocking=False):
            raise ValueError("Fleet queue is full; wait for existing jobs.")
        job = {"id": uuid.uuid4().hex, "host_id": host_id, "operation": operation, "name": name,
               "status": "queued", "created_at": datetime.now(timezone.utc).isoformat()}
        try:
            with self.jobs_lock:
                self.jobs[job["id"]] = job
                self._save(job)
            self.executor.submit(self._work, host, job["id"], operation, params)
        except Exception:
            self.pending.release()
            with self.jobs_lock:
                self.jobs.pop(job["id"], None)
                if self.state_dir:
                    (self.state_dir / (job["id"] + ".json")).unlink(missing_ok=True)
            raise
        return self.job(job["id"])

    def _work(self, host, job_id, operation, params):
        try:
            with self.locks[host.id]:
                with self.jobs_lock:
                    self.jobs[job_id]["status"] = "running"
                    self._save(self.jobs[job_id])
                started = time.monotonic()
                if operation == "install_template":
                    # The installer owns the same host flock internally and
                    # performs admission inside it. Never double-acquire it.
                    installed = self.registry.install(host, params["template"])
                    result = {key: installed[key] for key in ("id", "revision", "source", "snapshot", "installed_at")}
                else:
                    with host_operation_lock(host):
                        if operation == "create":
                            result = self._create(host, **params)
                        elif operation == "allocate":
                            result = self.warm.allocate(host, params["name"], params["template"], params.get("size"),
                                                        params.get("resources"), job_id=job_id)
                        else:
                            result = self._action(host, params["name"], operation)
                with self.jobs_lock:
                    self.jobs[job_id].update(status="succeeded", result=result,
                                             duration_seconds=round(time.monotonic() - started, 3))
                    self._save(self.jobs[job_id])
        except Exception as exc:
            with self.jobs_lock:
                self.jobs[job_id].update(status="failed", error=str(exc)[:2000])
                self._save(self.jobs[job_id])
        finally:
            self.pending.release()
            self.warm.wake(host.id)

    def _create(self, host, name, template, size=None, *, resources=None, pool_metadata=None, wait=True):
        facts = inventory(host)
        self.check_name(host, name, facts)
        source = self.template(host, template)
        config = self.resources(host, source, size, resources)
        if not resource_fits(host, facts, config):
            raise ValueError("Insufficient CPU, memory, disk budget or instance slots; preserve configured reserves.")
        box = host.box()
        snapshot = self.template_snapshot(host, source)
        root_name, template_root = root_device(box, snapshot, host.project)
        source_vm = next((vm for vm in facts["instances"] if vm["project"] == host.project and vm["name"] == source["source"]), None)
        source_disk = resource_bytes(template_root.get("size"), source_vm["disk_bytes"] if source_vm else 0)
        if not source_disk or config["disk_gib"] * GIB < source_disk:
            raise ValueError("Requested disk_gib must cover the template root disk; automatic shrinking is not supported.")
        started = time.monotonic()
        # Ownership and reserve metadata are committed as part of the copy, so
        # even partial failures remain visible to the next admission decision.
        metadata = []
        for key, value in (pool_metadata or {}).items():
            metadata.extend(["-c", f"user.enginebox.pool.{key}={value}"])
        box.run("copy", f"{source['source']}/{source['snapshot']}", name, "--stateless", "--storage", host.pool,
                "-c", f"limits.cpu={config['cpu']}", "-c", f"limits.memory={config['memory_gib']}GiB",
                "-c", "user.enginebox.fleet=true", "-c", f"user.enginebox.template={template}",
                "-c", "user.enginebox.clean-base=false", "-c", "boot.autostart=false", *metadata)
        copy_seconds = time.monotonic() - started
        clone = json.loads(box.run("query", f"/1.0/instances/{name}?project={quote(host.project, safe='')}"))
        root_name, clone_root = root_device(box, clone, host.project)
        if clone_root.get("pool") != host.pool:
            raise RuntimeError("Clone storage differs from the admitted pool; VM remains stopped for inspection.")
        operation = "set" if root_name in clone.get("devices", {}) else "override"
        box.run("config", "device", operation, name, root_name, f"size={config['disk_gib']}GiB",
                f"size.state={config['memory_gib'] + 2}GiB")
        box.run("start", name)
        readiness = self._wait_ready(box, name) if wait else None
        return {"name": name, "status": "Running", "copy_seconds": round(copy_seconds, 3),
                "ready_seconds": round(time.monotonic() - started, 3), "readiness": readiness}

    def _action(self, host, name, action):
        facts = inventory(host)
        vm = next((v for v in facts["instances"] if v["project"] == host.project and v["name"] == name), None)
        if not vm or not vm.get("managed"):
            raise ValueError("Fleet actions are limited to fleet-created VMs; use the explicit operator CLI for other workspaces.")
        if vm.get("warm_pool_state") in {"preparing", "ready", "quarantined"}:
            raise ValueError("Unassigned warm reserves cannot be changed through workspace lifecycle controls.")
        if vm.get("pool") != host.pool:
            raise ValueError("Workspace moved outside this host's admission pool; update the fleet configuration explicitly.")
        if action == "resume" and vm["status"] == "Stopped":
            room = capacity(host, facts)
            if vm["cpu"] > room["cpu_available"] or vm["memory_bytes"] > room["memory_budget_available_bytes"]:
                raise ValueError("Insufficient capacity to resume while preserving host reserves.")
        if action == "hibernate":
            room = capacity(host, facts)
            if vm.get("state_disk_bytes", 0) < vm["memory_bytes"] + GIB:
                raise ValueError("Configured memory-state disk is too small for this VM's RAM and overhead.")
            if min(room["pool_free_bytes"], room["disk_free_bytes"]) < vm["memory_bytes"] + host.reserve_disk_gib * GIB:
                raise ValueError("Insufficient free disk to hibernate while preserving reserve.")
        box = host.box()
        result = box.lifecycle(name, action)
        if action == "resume":
            result["readiness"] = self._wait_ready(box, name)
        return result

    @staticmethod
    def _wait_ready(box, name):
        deadline = time.monotonic() + 180
        expected = {key: getattr(box.guest, key) for key in ["uid", "gid", "user", "home", "workspace"]}
        last_error = "guest agent unavailable"
        while time.monotonic() < deadline:
            try:
                result = json.loads(box.execute(name, ["python3", "-c", READINESS_SCRIPT, json.dumps(expected)], timeout=15))
                if not isinstance(result, dict) or result.get("ready") is not True:
                    raise RuntimeError("Guest readiness probe returned an invalid result.")
                return result
            except (RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
                last_error = str(exc)[-500:]
                time.sleep(1)
        raise RuntimeError("VM started but identity, manifest, network and DNS were not ready within 180 seconds; inspect before retrying. " + last_error)
