"""Safe single-host Incus bootstrap. No agent harness or inference proxy."""
from __future__ import annotations

import argparse
from contextlib import contextmanager
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tarfile
import time

from bootstrap_config import fingerprint, load_config, normalize, runtime_environment, size_bytes


OWNER_KEY = "user.enginebox.owner"
RECIPE_KEY = "user.enginebox.recipe"
PREPARED_KEY = "user.enginebox.prepared"
PYTHON_ASSETS = ["bootstrap.py", "bootstrap_config.py", "enginebox.py", "mcp_server.py", "browser_tools.py",
                 "fleet.py", "fleet_api.py", "fleet_mcp.py"]
DATA_ASSETS = ["requirements.lock", "guest_setup.sh"]

# Runs using only the server's Python standard library. No files are written by
# inventory. Incus query always specifies the project explicitly where needed.
INVENTORY_SCRIPT = r'''
import fcntl, json, os, platform, shutil, stat, subprocess, sys
from pathlib import Path
from urllib.parse import quote
config = json.load(sys.stdin)
errors = []
def run(argv, required=False):
    try:
        p = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=45)
        if p.returncode:
            if required: errors.append(' '.join(argv[:3]) + ': ' + p.stderr.strip()[:500])
            return None
        return p.stdout
    except (OSError, subprocess.TimeoutExpired) as exc:
        if required: errors.append(str(exc))
        return None
def query(path, required=True):
    value = run(['incus', 'query', path], required)
    if value is None: return None
    try: return json.loads(value)
    except ValueError:
        errors.append('Non-JSON Incus response: ' + path)
        return None
release = {}
try:
    for line in Path('/etc/os-release').read_text().splitlines():
        if '=' in line:
            key, value = line.split('=', 1)
            release[key] = value.strip('"')
except OSError: pass
mem = {}
try:
    for line in Path('/proc/meminfo').read_text().splitlines():
        key, value = line.split(':', 1)
        mem[key] = int(value.split()[0]) * 1024
except OSError: pass
data_path = Path(config['host']['data_dir'])
while not data_path.exists() and data_path.parent != data_path:
    data_path = data_path.parent
disk = shutil.disk_usage(data_path)
incus_path = Path('/var/lib/incus')
while not incus_path.exists(): incus_path = incus_path.parent
incus_disk = shutil.disk_usage(incus_path)
has_incus = shutil.which('incus') is not None
server = query('/1.0') if has_incus else None
project = config['incus']['project']
projects = query('/1.0/projects?recursion=1') if server else []
projects = projects or []
project_exists = any(p.get('name') == project for p in projects)
project_query = '?project=' + quote(project, safe='') + '&recursion=1'
storage = (query('/1.0/storage-pools?recursion=1') or []) if server else []
pool = config['storage']['pool']
pool_resources = query('/1.0/storage-pools/' + quote(pool, safe='') + '/resources') if any(p.get('name') == pool for p in storage) else None
routes_text = run(['ip', '-j', '-4', 'route', 'show'])
try: routes = json.loads(routes_text) if routes_text else []
except ValueError: routes = []
firewalls = []
for tool in ['ufw', 'nft', 'iptables', 'docker']:
    if shutil.which(tool): firewalls.append(tool)
kvm = False
try:
    descriptor = os.open('/dev/kvm', os.O_RDWR | os.O_CLOEXEC)
    try:
        kvm = stat.S_ISCHR(os.fstat(descriptor).st_mode) and fcntl.ioctl(descriptor, 0xAE00, 0) == 12
    finally: os.close(descriptor)
except OSError: pass
result = {
    'system': platform.system(), 'architecture': platform.machine(), 'kernel': platform.release(),
    'python_version': list(sys.version_info[:3]),
    'os': release, 'uid': os.geteuid(), 'cpu_count': os.cpu_count() or 1,
    'memory_total': mem.get('MemTotal', 0), 'memory_available': mem.get('MemAvailable', 0),
    'host_disk_free': disk.free, 'incus_disk_free': incus_disk.free, 'data_filesystem': str(data_path),
    'kvm': kvm,
    'incus_present': has_incus, 'server': server,
    'qemu_version': run(['qemu-system-x86_64', '--version']),
    'projects': projects, 'storage': storage, 'pool_resources': pool_resources,
    'networks': (query('/1.0/networks?recursion=1') or []) if server else [],
    'profiles': (query('/1.0/profiles' + project_query) or []) if project_exists else [],
    'instances': (query('/1.0/instances' + project_query) or []) if project_exists else [],
    'routes': routes, 'firewall_tools': firewalls, 'errors': errors,
}
print(json.dumps(result))
'''


def transport_command(config: dict, argv: list[str]) -> list[str]:
    host = config["host"]
    command = argv if host["transport"] == "local" and os.geteuid() == 0 else ["sudo", "-n", *argv]
    if host["transport"] == "local":
        return command
    return ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10",
            host["target"], shlex.join(command)]


def inventory(config: dict) -> dict:
    result = subprocess.run(transport_command(config, ["python3", "-c", INVENTORY_SCRIPT]),
                            input=json.dumps(config).encode(), capture_output=True, timeout=180)
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace").strip() or "Host inspection failed.")
    try:
        return json.loads(result.stdout)
    except ValueError as exc:
        raise RuntimeError("Host inspection did not return JSON. Check SSH startup output and Python availability.") from exc


def owned(resource: dict, config: dict) -> bool:
    return resource.get("config", {}).get(OWNER_KEY) == config["incus"]["project"]


def profile_definition(config: dict) -> dict:
    ws = config["workspace"]
    return {
        "description": "Engine Box managed native-agent workspace",
        "config": {OWNER_KEY: config["incus"]["project"], "boot.autostart": "false",
                   "limits.cpu": str(ws["cpu"]), "limits.memory": ws["memory"],
                   "migration.stateful": "true",
                   "cloud-init.user-data": "#cloud-config\npreserve_hostname: true\n",
                   "cloud-init.network-config": "version: 2\nethernets:\n  enginebox:\n    match:\n      name: 'en*'\n    dhcp4: true\n    dhcp-identifier: mac\n"},
        "devices": {
            "root": {"type": "disk", "path": "/", "pool": config["storage"]["pool"],
                     "size": ws["disk"], "size.state": ws["state_disk"]},
            "eth0": {"type": "nic", "network": config["network"]["name"], "name": "eth0"},
        },
    }


def build_plan(config: dict, observed: dict) -> dict:
    """Pure desired-state planning. Never runs commands or silently adopts names."""
    import ipaddress
    import re
    errors = list(observed.get("errors", []))
    warnings = []
    operations = []
    host, ws = config["host"], config["workspace"]
    project, profile = config["incus"]["project"], config["incus"]["profile"]
    storage, network = config["storage"], config["network"]
    if observed.get("system") != "Linux" or observed.get("architecture") not in {"x86_64", "amd64"}:
        errors.append("Bootstrap supports Linux x86_64 hosts only.")
    if not observed.get("kvm"):
        errors.append("Host needs readable/writable /dev/kvm; nested virtualization must be enabled on VM hosts.")
    if observed.get("uid") != 0:
        errors.append("Inspection/apply requires administrative access through noninteractive sudo.")
    if tuple(observed.get("python_version", [])) < (3, 11):
        errors.append("Host Python 3.11+ is required by the portable bootstrap.")
    if ws["cpu"] > observed.get("cpu_count", 0):
        errors.append("workspace.cpu exceeds the host's logical CPU count.")
    if not observed.get("incus_present"):
        if not host["install_packages"]:
            errors.append("Incus is absent. Install it yourself or explicitly set host.install_packages = true.")
        elif observed.get("os", {}).get("ID") != "ubuntu" or observed.get("os", {}).get("VERSION_ID") not in {"24.04", "26.04"}:
            errors.append("Automatic package installation supports Ubuntu 24.04/26.04 only.")
        else:
            operations.append({"action": "install_packages"})
            warnings.append("Package candidates are checked before installation; actual Incus capabilities are rechecked afterward.")
    elif not observed.get("server"):
        errors.append("Incus is installed but its daemon is unavailable. Start/repair it explicitly before applying.")
    else:
        environment = observed["server"].get("environment", {})
        version = environment.get("server_version", "")
        match = re.match(r"([0-9]+)\.([0-9]+)", version)
        if not match or tuple(map(int, match.groups())) < (6, 0):
            errors.append(f"Incus >=6.0 is required; observed {version!r}.")
        if "migration_stateful" not in observed["server"].get("api_extensions", []):
            errors.append("Incus does not advertise stateful VM migration capability.")
        drivers = environment.get("driver", "")
        if drivers and "qemu" not in drivers:
            errors.append("Incus does not advertise a QEMU VM driver.")
        qemu = re.search(r"version ([0-9]+)\.([0-9]+)", observed.get("qemu_version") or "")
        if not qemu or tuple(map(int, qemu.groups())) < (8, 2):
            errors.append("QEMU >=8.2 is required for this bootstrap target.")
    resources = {key: {item["name"]: item for item in observed.get(key, [])} for key in ["projects", "storage", "networks", "profiles", "instances"]}
    instance = resources["instances"].get(ws["name"])
    if instance is None or (instance.get("status") == "Stopped" and instance.get("config", {}).get(PREPARED_KEY) != fingerprint(config)):
        required_memory = size_bytes(ws["memory"]) + size_bytes(host["memory_reserve"])
        if observed.get("memory_available", 0) < required_memory:
            errors.append("Available RAM is below workspace.memory plus host.memory_reserve.")
    required_disk = size_bytes(ws["disk"]) + size_bytes(ws["state_disk"]) + size_bytes(host["disk_reserve"])
    pool = resources["storage"].get(storage["pool"])
    if pool is None:
        if storage["mode"] == "existing":
            errors.append(f"Existing storage pool {storage['pool']!r} was not found.")
        else:
            operations.append({"action": "create_pool"})
            # Incus loop disks are in /var/lib/incus; inventory also checks that
            # filesystem in apply before allocating, not only the journal path.
            if observed.get("incus_disk_free", 0) < size_bytes(storage["size"]) + size_bytes(host["disk_reserve"]):
                errors.append("Host free disk is below the loop pool bound plus disk reserve.")
    elif storage["mode"] == "development-loop":
        if not owned(pool, config):
            errors.append(f"Storage pool {storage['pool']!r} exists but is not owned by this installation.")
        elif pool.get("driver") != "btrfs" or pool.get("config", {}).get("size") != storage["size"]:
            errors.append("Managed storage pool configuration differs; resize/migrate explicitly.")
    if pool and pool.get("driver") not in {"btrfs", "zfs", "lvm", "dir"}:
        errors.append("Initial bootstrap accepts existing btrfs/zfs/lvm/dir storage pools only.")
    if pool and pool.get("driver") == "dir":
        warnings.append("The dir storage driver does not provide efficient copy-on-write forks.")
    if pool and instance is None:
        space = (observed.get("pool_resources") or {}).get("space", {})
        if not space or space.get("total", 0) - space.get("used", 0) < required_disk:
            errors.append("Storage pool has insufficient free capacity for disk, saved memory and reserve.")
        source = pool.get("config", {}).get("source", "")
        if source.endswith(".img") and observed.get("incus_disk_free", 0) < required_disk:
            errors.append("Loop pool backing filesystem has insufficient space for disk, saved memory and reserve.")
        if observed.get("host_disk_free", 0) < size_bytes(host["disk_reserve"]):
            errors.append("Host filesystem is below host.disk_reserve.")
    net = resources["networks"].get(network["name"])
    if net is None:
        if network["mode"] == "existing":
            errors.append(f"Existing network {network['name']!r} was not found.")
        else:
            wanted = ipaddress.IPv4Interface(network["subnet"]).network
            for route in observed.get("routes", []):
                destination = route.get("dst", "default")
                if destination == "default":
                    continue
                try:
                    if wanted.overlaps(ipaddress.IPv4Network(destination, strict=False)):
                        errors.append(f"Managed subnet overlaps host route {destination}.")
                except ValueError:
                    continue
            for other in resources["networks"].values():
                address = other.get("config", {}).get("ipv4.address", "none")
                try:
                    if wanted.overlaps(ipaddress.IPv4Interface(address).network):
                        errors.append(f"Managed subnet overlaps Incus network {other['name']}.")
                except ValueError:
                    continue
            operations.append({"action": "create_network"})
    else:
        if not net.get("managed") or net.get("type") != "bridge":
            errors.append("The selected existing network must be an Incus managed bridge.")
        if network["mode"] == "managed":
            if not owned(net, config):
                errors.append(f"Network {network['name']!r} exists but is not owned by this installation.")
            elif net.get("config", {}).get("ipv4.address") != network["subnet"]:
                errors.append("Managed network subnet differs; migrate explicitly rather than replacing it.")
        if net.get("config", {}).get("ipv4.address", "none") in {"none", "auto"} or net.get("config", {}).get("ipv4.dhcp", "true") != "true":
            errors.append("Selected network needs a concrete IPv4 address and DHCP enabled.")
    existing_project = resources["projects"].get(project)
    if existing_project and not owned(existing_project, config):
        errors.append(f"Project {project!r} already exists without Engine Box ownership. Select a new project.")
    elif existing_project:
        project_config = existing_project.get("config", {})
        if project_config.get("features.networks", "false") != "false" or project_config.get("features.profiles", "true") != "true":
            errors.append("Owned project must use global networks and private profiles.")
    else:
        operations.append({"action": "create_project"})
    current_profile = resources["profiles"].get(profile)
    expected_profile = profile_definition(config)
    if current_profile:
        if not owned(current_profile, config):
            errors.append(f"Profile {profile!r} exists without Engine Box ownership.")
        elif current_profile.get("config") != expected_profile["config"] or current_profile.get("devices") != expected_profile["devices"]:
            errors.append("Owned profile differs from requested configuration. Use a new profile/workspace; upgrades are explicit.")
    else:
        operations.append({"action": "create_profile"})
    if instance:
        if not owned(instance, config):
            errors.append(f"Workspace {ws['name']!r} exists without Engine Box ownership.")
        elif instance.get("config", {}).get(RECIPE_KEY) != fingerprint(config):
            errors.append("Workspace configuration has changed. Use a new name or perform an explicit migration.")
        elif instance.get("profiles") != [profile] or instance.get("type") != "virtual-machine":
            errors.append("Workspace profile/type changed outside bootstrap; inspect it before continuing.")
        elif instance.get("devices") or any(instance.get("expanded_config", {}).get(key, value) != value for key, value in expected_profile["config"].items()):
            errors.append("Workspace overrides the managed profile or adds devices; inspect stateful compatibility before continuing.")
        elif instance.get("config", {}).get(PREPARED_KEY) != fingerprint(config):
            operations.append({"action": "prepare_workspace"})
    else:
        operations.extend([{"action": "create_workspace"}, {"action": "prepare_workspace"}])
    if observed.get("firewall_tools"):
        warnings.append("Host firewall/Docker configuration is preserved. Existing policies must permit guest DHCP/DNS and egress; bootstrap never changes global rules.")
    if not config["modules"]["desktop"] and config["modules"]["browser"]:
        warnings.append("Browser installation is enabled; headed computer use requires desktop=true.")
    return {"schema_version": 1, "config_fingerprint": fingerprint(config), "ok": not errors,
            "errors": errors, "warnings": warnings, "operations": operations,
            "runtime_environment": runtime_environment(config)}


def run_command(argv: list[str], *, input: bytes | None = None, timeout: int = 1800) -> bytes:
    process = subprocess.run(argv, input=input, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    if process.stderr:
        sys.stderr.buffer.write(process.stderr)
        sys.stderr.buffer.flush()
    if process.returncode:
        raise RuntimeError(f"Command failed ({process.returncode}): {shlex.join(argv[:5])}\n" + process.stdout.decode(errors="replace")[-1500:])
    return process.stdout


def incus(config: dict, *argv: str, project: bool = False, input: bytes | None = None, timeout: int = 1800) -> bytes:
    return run_command(["incus", *(["--project", config["incus"]["project"]] if project else []), *argv], input=input, timeout=timeout)


@contextmanager
def host_lock(config: dict):
    import fcntl
    # One lock across all configs prevents two projects planning the same pool,
    # subnet or remaining capacity concurrently. Never follows a user symlink.
    lock = Path("/run/lock/enginebox-host.lock")
    fd = os.open(lock, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        if os.fstat(fd).st_uid != 0 or os.fstat(fd).st_mode & 0o022:
            raise RuntimeError("Host lock must be root-owned and not group/world writable.")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise RuntimeError("Another Engine Box bootstrap holds the host lock.") from exc
        yield
    finally:
        os.close(fd)


def write_journal(config: dict, journal: dict) -> None:
    root = Path(config["host"]["data_dir"])
    if root.resolve() != root:
        raise RuntimeError("host.data_dir must not traverse symbolic links.")
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    if root.stat().st_uid != 0 or root.stat().st_mode & 0o022:
        raise RuntimeError("host.data_dir must be root-owned and not group/world writable.")
    path = root / (config["incus"]["project"] + "-bootstrap.json")
    temp = root / ("." + path.name + ".tmp")
    fd = os.open(temp, os.O_CREAT | os.O_WRONLY | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(journal, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    temp.replace(path)


def install_packages(config: dict) -> None:
    # Only called when Incus is absent and explicit installation was requested.
    # APT itself verifies repository metadata/signatures. Do not add repositories.
    packages = ["incus", "qemu-system-x86", "qemu-system-modules-spice", "qemu-utils", "ovmf", "btrfs-progs"]
    environment = {**os.environ, "DEBIAN_FRONTEND": "noninteractive", "NEEDRESTART_MODE": "l"}
    for command in [["apt-get", "update"], ["apt-get", "install", "-y", "--no-install-recommends", *packages]]:
        if command[1] == "install":
            import re
            for package, minimum in [("incus", (6, 0)), ("qemu-system-x86", (8, 2))]:
                policy = run_command(["apt-cache", "policy", package]).decode()
                candidate = re.search(r"Candidate:\s+(?:[0-9]+:)?([0-9]+)\.([0-9]+)", policy)
                if not candidate or tuple(map(int, candidate.groups())) < minimum:
                    raise RuntimeError(f"APT candidate for {package} does not meet the supported version floor; no packages installed.")
        result = subprocess.run(command, env=environment, stdout=sys.stderr, stderr=sys.stderr, timeout=1800)
        if result.returncode:
            raise RuntimeError("APT package installation failed; inspect output and rerun after repair.")
    run_command(["systemctl", "start", "incus.socket"])


def wait_guest(config: dict) -> None:
    deadline = time.monotonic() + config["workspace"]["ready_timeout"]
    last = "guest agent unavailable"
    while time.monotonic() < deadline:
        try:
            incus(config, "exec", config["workspace"]["name"], "--", "true", project=True, timeout=15)
            return
        except (RuntimeError, subprocess.TimeoutExpired) as exc:
            last = str(exc)
            time.sleep(2)
    raise RuntimeError("Guest did not become ready; existing firewall/routing is not modified automatically. " + last[-500:])


def guest_environment(config: dict) -> dict[str, str]:
    ws, modules = config["workspace"], config["modules"]
    result = {"EBOX_USER": ws["user"], "EBOX_UID": str(ws["uid"]), "EBOX_GID": str(ws["gid"]),
              "EBOX_HOME": ws["home"], "EBOX_WORKSPACE": ws["path"],
              "EBOX_DESKTOP": str(int(modules["desktop"])),
              "EBOX_MCP": str(int(modules["guest_computer_mcp"] or modules["browser"])),
              "EBOX_COMPUTER_MCP": str(int(modules["guest_computer_mcp"])),
              "EBOX_BROWSER": str(int(modules["browser"]))}
    result.update({"EBOX_INSTALL_" + client.upper(): str(int(client in config["agents"]["install"])) for client in ["codex", "claude", "cursor"]})
    return result


CLEAN_BASE_CHECK = r'''
import json, os
from pathlib import Path
home = Path(os.environ['EBOX_HOME'])
blocked = []
for relative in ['.codex/auth.json', '.claude/.credentials.json', '.cursor/auth.json',
                 '.config/cursor/auth.json', '.git-credentials', '.netrc', '.aws/credentials']:
    if (home / relative).exists(): blocked.append(relative)
paths = [home / '.claude.json']
for directory in ['.codex', '.claude', '.cursor', '.config/cursor']:
    root = home / directory
    if not root.exists(): continue
    paths.extend(root.glob('*.json'))
for path in paths:
    try: data = json.loads(path.read_text())
    except (OSError, ValueError): continue
    def contains_secret(item):
        if isinstance(item, dict):
            for key, value in item.items():
                normalized = ''.join(c for c in key.lower() if c.isalnum())
                if normalized in {'accesstoken', 'refreshtoken', 'apikey', 'token', 'authorization', 'oauthaccount', 'clientsecret'} and value:
                    return True
                if contains_secret(value): return True
        elif isinstance(item, list):
            return any(contains_secret(value) for value in item)
        return False
    if contains_secret(data): blocked.append(str(path.relative_to(home)))
for path in (home / '.ssh').glob('id_*'):
    if path.is_file() and not path.name.endswith('.pub'): blocked.append(str(path.relative_to(home)))
for root in [home / '.local/share/enginebox', home / '.enginebox']:
    if root.exists():
        for profile in root.glob('**/profile'):
            if profile.is_dir() and any(profile.iterdir()): blocked.append(str(profile.relative_to(home)))
if blocked:
    raise SystemExit('Refusing clean-base snapshot: account/credential state found at ' + ', '.join(sorted(set(blocked))))
'''


def prepare_workspace(config: dict, source: Path) -> None:
    name = config["workspace"]["name"]
    state = json.loads(incus(config, "query", f"/1.0/instances/{name}?project={config['incus']['project']}"))
    # A process killed after snapshot labelling can leave this temporary marker
    # on the source. Only the immutable pre-login snapshot may retain it.
    incus(config, "config", "unset", name, "user.enginebox.clean-base", project=True)
    if state["status"] == "Stopped" and not state.get("stateful"):
        incus(config, "start", name, project=True)
    elif state["status"] != "Running":
        raise RuntimeError("An incomplete workspace is frozen/hibernated. Resume it explicitly before preparation.")
    wait_guest(config)
    incus(config, "exec", name, "--", "mkdir", "-p", "/opt/enginebox-runtime", project=True)
    required = ["enginebox.py", "mcp_server.py", "browser_tools.py", "requirements.lock", "guest_setup.sh"]
    for filename in required:
        if not (source / filename).is_file():
            raise RuntimeError(f"Missing provisioning asset {filename}.")
    for filename in PYTHON_ASSETS + DATA_ASSETS:
        path = source / filename
        if path.is_file():
            incus(config, "file", "push", str(path), f"{name}/opt/enginebox-runtime/{path.name}", project=True)
    env = [f"--env={key}={value}" for key, value in guest_environment(config).items()]
    setup_output = incus(config, "exec", name, *env, "--", "bash", "/opt/enginebox-runtime/guest_setup.sh", project=True, timeout=3600)
    if setup_output:
        sys.stderr.buffer.write(setup_output)
    # The prepared base is captured before login; bootstrap never imports an
    # existing VM or copies operator account state into it.
    incus(config, "exec", name, f"--env=EBOX_HOME={config['workspace']['home']}", "--", "python3", "-c", CLEAN_BASE_CHECK, project=True)
    # Only the owned, pre-login base reaches this point. An empty machine-id
    # makes first-boot PID 1 initialize a fresh identity in each cold clone;
    # the ordinary guest installer must not reset IDs in an active workspace.
    reset_identity = "from pathlib import Path; Path('/etc/machine-id').write_text(''); p=Path('/var/lib/dbus/machine-id'); p.write_text('') if p.exists() and not p.is_symlink() else None"
    incus(config, "exec", name, "--", "python3", "-c", reset_identity, project=True)
    incus(config, "stop", name, "--timeout=120", project=True)
    snapshot = "prepared-" + fingerprint(config)[:12]
    snapshots = json.loads(incus(config, "query", f"/1.0/instances/{name}/snapshots?project={config['incus']['project']}&recursion=1"))
    if not any(item["name"].rsplit("/", 1)[-1] == snapshot for item in snapshots):
        incus(config, "config", "set", name, "user.enginebox.clean-base=true", project=True)
        try:
            incus(config, "snapshot", "create", name, snapshot, project=True)
        finally:
            # Only the immutable pre-login snapshot is clean. Future user-created
            # snapshots must not inherit this label after a provider login.
            incus(config, "config", "unset", name, "user.enginebox.clean-base", project=True)
    incus(config, "config", "unset", name, "user.enginebox.clean-base", project=True)
    incus(config, "start", name, project=True)
    wait_guest(config)
    # Completion must follow successful restart/readiness. Otherwise a failed
    # restart would make the next apply incorrectly report a completed no-op.
    incus(config, "config", "set", name, f"{PREPARED_KEY}={fingerprint(config)}", project=True)


def execute_operation(config: dict, action: str, source: Path) -> None:
    storage, network, ws = config["storage"], config["network"], config["workspace"]
    owner = f"{OWNER_KEY}={config['incus']['project']}"
    if action == "install_packages":
        install_packages(config)
    elif action == "create_pool":
        import shutil
        pool_parent = Path("/var/lib/incus")
        while not pool_parent.exists():
            pool_parent = pool_parent.parent
        if shutil.disk_usage(pool_parent).free < size_bytes(storage["size"]) + size_bytes(config["host"]["disk_reserve"]):
            raise RuntimeError("Insufficient free space on the Incus loop-storage filesystem.")
        incus(config, "storage", "create", storage["pool"], "btrfs", f"size={storage['size']}", owner)
    elif action == "create_network":
        incus(config, "network", "create", network["name"], f"ipv4.address={network['subnet']}",
              "ipv4.nat=true", "ipv6.address=none", owner)
    elif action == "create_project":
        incus(config, "project", "create", config["incus"]["project"], "-c", owner,
              "-c", "features.images=false", "-c", "features.profiles=true",
              "-c", "features.storage.volumes=false", "-c", "features.networks=false")
    elif action == "create_profile":
        # POST creates profile and ownership atomically. A crash cannot leave an
        # unmarked profile that a later run mistakes for unrelated resources.
        body = {"name": config["incus"]["profile"], **profile_definition(config)}
        incus(config, "query", "--request", "POST", f"/1.0/profiles?project={config['incus']['project']}",
              "--data", json.dumps(body))
    elif action == "create_workspace":
        incus(config, "init", ws["image"], ws["name"], "--vm", "--profile", config["incus"]["profile"],
              "-c", owner, "-c", f"{RECIPE_KEY}={fingerprint(config)}", project=True)
    elif action == "prepare_workspace":
        prepare_workspace(config, source)
    else:
        raise ValueError(f"Unknown bootstrap operation: {action}")


def apply_local(config: dict, source: Path) -> dict:
    if os.geteuid() != 0:
        raise RuntimeError("Host apply must run as root.")
    local_config = {**config, "host": {**config["host"], "transport": "local"}}
    journal = {"schema_version": 1, "config_fingerprint": fingerprint(config), "completed": [], "status": "running"}
    with host_lock(config):
        try:
            # Replan after every operation: ownership and capacity remain checked
            # even after interrupted runs or package installation reveals Incus.
            for _ in range(16):
                plan = build_plan(config, inventory(local_config))
                if not plan["ok"]:
                    raise RuntimeError("Preflight failed: " + "; ".join(plan["errors"]))
                if not plan["operations"]:
                    journal["status"] = "complete"
                    write_journal(config, journal)
                    return {"ok": True, "changed": bool(journal["completed"]),
                            "completed": journal["completed"], "runtime_environment": runtime_environment(config),
                            "workspace": config["workspace"]["name"], "snapshot": "prepared-" + fingerprint(config)[:12],
                            "warnings": plan["warnings"]}
                action = plan["operations"][0]["action"]
                journal["current"] = action
                write_journal(config, journal)
                print(f"Engine Box: {action}", file=sys.stderr, flush=True)
                execute_operation(config, action, source)
                journal["completed"].append(action)
                write_journal(config, journal)
            raise RuntimeError("Bootstrap did not converge after 16 operations.")
        except Exception as exc:
            journal["status"] = "failed"
            journal["error"] = str(exc)[:2000]
            write_journal(config, journal)
            raise


EXTRACT_AND_RUN = r'''
import io, os, pathlib, subprocess, sys, tarfile, tempfile
with tempfile.TemporaryDirectory(prefix='enginebox-apply-') as directory:
    root = pathlib.Path(directory)
    with tarfile.open(fileobj=io.BytesIO(sys.stdin.buffer.read()), mode='r:gz') as archive:
        for member in archive.getmembers():
            if not member.isfile() or '/' in member.name or member.name in {'.', '..'}:
                raise SystemExit('Invalid bootstrap bundle member')
            stream = archive.extractfile(member)
            (root / member.name).write_bytes(stream.read())
    result = subprocess.run([sys.executable, str(root / 'bootstrap.py'), '_host_apply', '--config-json', str(root / 'config.json')], stdin=subprocess.DEVNULL)
    raise SystemExit(result.returncode)
'''


def make_bundle(config: dict) -> bytes:
    source = Path(__file__).resolve().parent
    required = ["bootstrap.py", "bootstrap_config.py", "enginebox.py", "mcp_server.py", "browser_tools.py", "requirements.lock", "guest_setup.sh"]
    assets = {}
    for filename in PYTHON_ASSETS + DATA_ASSETS:
        path = source / filename
        if not path.is_file() and filename in DATA_ASSETS:
            path = Path(sys.prefix) / "share" / "enginebox" / filename
        if path.is_file():
            assets[filename] = path
        elif filename in required:
            raise RuntimeError(f"Missing required runtime asset {filename}; use a complete Engine Box checkout/install.")
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for filename, path in sorted(assets.items()):
            archive.add(path, arcname=filename, recursive=False)
        contents = json.dumps(config).encode()
        info = tarfile.TarInfo("config.json")
        info.size, info.mode = len(contents), 0o600
        archive.addfile(info, io.BytesIO(contents))
    return output.getvalue()


def apply(config: dict) -> dict:
    # Read-only inspection before creating even temporary remote files.
    plan = build_plan(config, inventory(config))
    if not plan["ok"]:
        raise RuntimeError("Preflight failed: " + "; ".join(plan["errors"]))
    if not plan["operations"]:
        return {"ok": True, "changed": False, "completed": [], "runtime_environment": runtime_environment(config),
                "workspace": config["workspace"]["name"], "snapshot": "prepared-" + fingerprint(config)[:12],
                "warnings": plan["warnings"]}
    result = subprocess.run(transport_command(config, ["python3", "-c", EXTRACT_AND_RUN]),
                            input=make_bundle(config), stdout=subprocess.PIPE, timeout=7200)
    if result.returncode:
        raise RuntimeError("Bootstrap failed. Completed resources are retained; inspect diagnostics and rerun the same config to resume.")
    return json.loads(result.stdout)


EXAMPLE = '''# Engine Box bootstrap v1. No credentials belong in this file.
schema_version = 1
[host]
transport = "ssh"
target = "my-server"
install_packages = true # Explicit opt-in; Ubuntu 24.04/26.04 only
[incus]
project = "enginebox"
profile = "enginebox"
[storage]
mode = "development-loop" # Development convenience; no repartitioning
pool = "ebox-pool"
size = "120GiB"
[network]
mode = "managed"
name = "eboxbr0"
subnet = "10.203.0.1/24"
[workspace]
name = "ebox-dev"
image = "images:ubuntu/24.04/cloud"
cpu = 4
memory = "4GiB"
disk = "30GiB"
state_disk = "8GiB"
user = "agent"
uid = 1001
gid = 1001
home = "/home/agent"
path = "/workspace"
[agents]
install = ["codex", "cursor", "claude"]
[modules]
desktop = false
browser = true
operator_mcp = true
guest_computer_mcp = false
ci = false
'''


def run_cli(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    subs = parser.add_subparsers(dest="action", required=True)
    init = subs.add_parser("init", help="Write a config example; no host access")
    init.add_argument("--output", default="enginebox.toml", type=Path)
    for action in ["doctor", "plan", "apply"]:
        command = subs.add_parser(action)
        command.add_argument("--config", required=True, type=Path)
    internal = subs.add_parser("_host_apply", help=argparse.SUPPRESS)
    internal.add_argument("--config-json", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        if args.action == "init":
            with args.output.open("x") as stream:
                stream.write(EXAMPLE)
            result = {"created": str(args.output), "next": "Edit host and resource names, then run doctor/plan."}
        elif args.action == "_host_apply":
            config = normalize(json.loads(args.config_json.read_text()))
            result = apply_local(config, Path(__file__).resolve().parent)
        else:
            config = load_config(args.config)
            if args.action == "apply":
                result = apply(config)
            else:
                observed = inventory(config)
                result = build_plan(config, observed)
                if args.action == "doctor":
                    result["host"] = {key: observed.get(key) for key in ["system", "architecture", "kernel", "os", "python_version", "kvm", "cpu_count", "memory_available", "host_disk_free"]}
            print(json.dumps(result, indent=2))
            if not result.get("ok", True):
                parser.exit(1)
            return
        print(json.dumps(result, indent=2))
    except (ValueError, RuntimeError, OSError, subprocess.TimeoutExpired) as exc:
        parser.exit(1, f"Engine Box bootstrap: {exc}\n")


if __name__ == "__main__":
    run_cli()
