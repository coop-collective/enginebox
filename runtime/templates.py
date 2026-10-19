"""Portable template manifests and deployment receipts; no imported executable code."""
from __future__ import annotations

from contextlib import contextmanager
from datetime import datetime, timezone
import fcntl
import hashlib
import http.client
import ipaddress
import json
import os
from pathlib import Path
import re
import socket
import ssl
import stat
import threading
import time
from urllib.parse import quote, urlsplit
import uuid

import bootstrap
from bootstrap_config import fingerprint, normalize


MAX_MANIFEST_BYTES = 1024 * 1024
IMPORT_TIMEOUT = 15
RECIPE = "native-clients-v1"
IMAGE = "images:ubuntu/24.04/cloud"
RESOURCE_KEYS = {"cpu", "memory_gib", "disk_gib"}
MANIFEST_KEYS = {"schema_version", "id", "revision", "label", "description", "recipe", "source_image",
                 "minimum", "recommended", "native_clients", "config"}


def _canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def _digest(value):
    return hashlib.sha256(_canonical(value)).hexdigest()


def _id(value):
    if not isinstance(value, str) or not re.fullmatch(r"[a-z][a-z0-9-]{0,31}", value):
        raise ValueError("Template id must be 1–32 lowercase letters, digits or hyphens, starting with a letter.")
    return value


def _object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"Duplicate JSON key: {key}")
        result[key] = value
    return result


def _decode(raw):
    if isinstance(raw, str):
        raw = raw.encode()
    if isinstance(raw, bytes):
        if len(raw) > MAX_MANIFEST_BYTES:
            raise ValueError("Template JSON exceeds 1 MiB.")
        try:
            raw = json.loads(raw.decode("utf-8"), object_pairs_hook=_object,
                             parse_constant=lambda _value: (_ for _ in ()).throw(ValueError("Non-finite JSON number.")))
        except (UnicodeError, json.JSONDecodeError, RecursionError) as exc:
            raise ValueError("Template must contain valid UTF-8 JSON.") from exc
    if not isinstance(raw, dict):
        raise ValueError("Template must be a JSON object.")
    try:
        if len(_canonical(raw)) > MAX_MANIFEST_BYTES:
            raise ValueError("Template JSON exceeds 1 MiB.")
    except (TypeError, RecursionError, UnicodeError) as exc:
        raise ValueError("Template must contain serializable JSON values.") from exc
    return raw


def validate_resources(resources, minimum=None):
    """Validate an exact CPU/RAM/root-disk request, optionally against a template floor."""
    if not isinstance(resources, dict) or set(resources) != RESOURCE_KEYS:
        raise ValueError("Resources require exactly cpu, memory_gib and disk_gib.")
    bounds = {"cpu": (1, 512), "memory_gib": (1, 512), "disk_gib": (12, 16384)}
    result = {}
    for key, (low, high) in bounds.items():
        value = resources[key]
        if type(value) is not int or not low <= value <= high:
            raise ValueError(f"{key} must be an integer between {low} and {high}.")
        result[key] = value
    if minimum is not None:
        floor = validate_resources(minimum)
        for key in RESOURCE_KEYS:
            if result[key] < floor[key]:
                raise ValueError(f"{key} must be at least the template minimum of {floor[key]}.")
    return result


def validate_manifest(raw):
    raw = _decode(raw)
    required = {"schema_version", "id", "revision", "label", "minimum", "recommended"}
    if set(raw) - MANIFEST_KEYS or not required <= set(raw):
        raise ValueError("Template requires schema_version/id/revision/label/minimum/recommended and only supported fields.")
    if type(raw["schema_version"]) is not int or raw["schema_version"] != 1:
        raise ValueError("Only template schema_version 1 is supported.")
    _id(raw["id"])
    if type(raw["revision"]) is not int or not 1 <= raw["revision"] <= 2147483647:
        raise ValueError("Template revision must be a positive integer.")
    for key, limit in [("label", 120), ("description", 2000)]:
        value = raw.get(key, "")
        if not isinstance(value, str) or len(value) > limit or any(ord(c) < 32 and c != "\n" for c in value):
            raise ValueError(f"Template {key} must be text of at most {limit} characters, without control characters.")
    if not raw["label"].strip() or "\n" in raw["label"]:
        raise ValueError("Template label must be nonempty single-line text.")
    if raw.get("recipe", RECIPE) != RECIPE or raw.get("source_image", IMAGE) != IMAGE:
        raise ValueError("Only native-clients-v1 with images:ubuntu/24.04/cloud is supported.")
    minimum = validate_resources(raw["minimum"])
    recommended = validate_resources(raw["recommended"], minimum)
    clients = raw.get("native_clients", ["codex", "cursor", "claude"])
    if (not isinstance(clients, list) or any(type(c) is not str or c not in {"codex", "cursor", "claude"} for c in clients)
            or len(set(clients)) != len(clients)):
        raise ValueError("native_clients must be a unique list drawn from codex, cursor and claude.")
    options = raw.get("config", {})
    defaults = {"browser": True, "desktop": False, "guest_computer_mcp": False}
    if not isinstance(options, dict) or set(options) - set(defaults) or any(type(v) is not bool for v in options.values()):
        raise ValueError("Template config accepts only boolean browser, desktop and guest_computer_mcp.")
    options = defaults | options
    if options["guest_computer_mcp"] and not options["desktop"]:
        raise ValueError("guest_computer_mcp requires desktop=true.")
    return {"schema_version": 1, "id": raw["id"], "revision": raw["revision"], "label": raw["label"],
            "description": raw.get("description", ""), "recipe": RECIPE, "source_image": IMAGE,
            "minimum": minimum, "recommended": recommended, "native_clients": sorted(clients), "config": options}


def resolve_resources(manifest, resources=None, *, preset="recommended"):
    """Resolve a known minimum/recommended preset or a complete custom request."""
    if preset not in {"minimum", "recommended"}:
        raise ValueError("Template resource preset must be minimum or recommended.")
    return validate_resources(manifest[preset] if resources is None else resources, manifest["minimum"])


def host_binding(host):
    """Bind receipts to host identity and guest access, not merely a reusable host ID."""
    guest = {key: getattr(host.guest, key) for key in ["user", "uid", "gid", "home", "workspace", "display"]}
    return {"target": host.target, "project": host.project, "pool": host.pool, "guest": guest,
            "network": getattr(host, "network", "") or ""}


def host_binding_digest(host):
    return _digest(host_binding(host))


class _PinnedHTTPSConnection(http.client.HTTPSConnection):
    def __init__(self, hostname, address, port, timeout):
        super().__init__(hostname, port=port, timeout=timeout, context=ssl.create_default_context())
        self.address = address

    def connect(self):
        sock = socket.create_connection((self.address, self.port), timeout=self.timeout)
        try:
            self.sock = self._context.wrap_socket(sock, server_hostname=self.host)
        except BaseException:
            sock.close()
            raise


def fetch_manifest(url):
    """Read one public HTTPS document; no redirects, proxies, cookies or URL credentials."""
    if not isinstance(url, str) or len(url) > 2048 or any(ord(c) <= 32 for c in url):
        raise ValueError("Template URL must be a bounded HTTPS URL without whitespace.")
    try:
        parsed = urlsplit(url)
        port = parsed.port or 443
        hostname = parsed.hostname
        if (parsed.scheme != "https" or not hostname or parsed.username is not None or parsed.password is not None
                or parsed.fragment or port != 443):
            raise ValueError
    except ValueError as exc:
        raise ValueError("Template URL requires HTTPS port 443, with no credentials or fragment.") from exc
    deadline = time.monotonic() + IMPORT_TIMEOUT
    try:
        addresses = list(dict.fromkeys(item[4][0] for item in socket.getaddrinfo(hostname, port, type=socket.SOCK_STREAM)))
        if not addresses or any(not ipaddress.ip_address(address).is_global for address in addresses):
            raise ValueError("Template URLs must resolve only to public Internet addresses.")
        # Pin the validated IP while preserving hostname verification and SNI.
        # This avoids a second DNS resolution into a private network.
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError
        connection = _PinnedHTTPSConnection(hostname, addresses[0], port, remaining)
        try:
            path = parsed.path or "/"
            if parsed.query:
                path += "?" + parsed.query
            connection.request("GET", path, headers={"Accept": "application/json", "Accept-Encoding": "identity",
                                                     "User-Agent": "EngineBox-template-import/1"})
            response = connection.getresponse()
            if response.status != 200:
                raise ValueError(f"Template download returned HTTP {response.status}; redirects are not followed.")
            if response.getheader("Content-Encoding", "identity").lower() != "identity":
                raise ValueError("Compressed template responses are not accepted.")
            declared = response.getheader("Content-Length")
            if declared is not None and (not declared.isdigit() or int(declared) > MAX_MANIFEST_BYTES):
                raise ValueError("Template response exceeds 1 MiB or has an invalid Content-Length.")
            chunks, length = [], 0
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError
                if connection.sock is not None:
                    connection.sock.settimeout(remaining)
                chunk = response.read1(min(65536, MAX_MANIFEST_BYTES + 1 - length))
                if not chunk:
                    break
                chunks.append(chunk)
                length += len(chunk)
                if length > MAX_MANIFEST_BYTES:
                    raise ValueError("Template response exceeds 1 MiB.")
            return b"".join(chunks)
        finally:
            connection.close()
    except (OSError, http.client.HTTPException, UnicodeError) as exc:
        # Do not echo URL query values or response content into API errors/logs.
        raise ValueError("Template HTTPS download failed or timed out.") from exc


def discover_network(host):
    explicit = getattr(host, "network", "")
    if explicit:
        return explicit
    candidates = set()
    box = host.box()
    for template in host.templates:
        vm = json.loads(box.run("query", f"/1.0/instances/{quote(template['source'], safe='')}?project={quote(host.project, safe='')}"))
        devices = vm.get("expanded_devices")
        if devices is None:
            devices = {}
            for profile in vm.get("profiles", []):
                value = json.loads(box.run("query", f"/1.0/profiles/{quote(profile, safe='')}?project={quote(host.project, safe='')}"))
                devices.update(value.get("devices", {}))
            devices.update(vm.get("devices", {}))
        for device in devices.values():
            if device.get("type") == "nic":
                network = device.get("network") or (device.get("parent") if device.get("nictype") == "bridged" else None)
                if network:
                    candidates.add(network)
    if len(candidates) != 1:
        raise ValueError("Set the host network explicitly; existing template bases do not identify one unambiguous bridge.")
    return candidates.pop()


def bootstrap_configuration(host, manifest, network):
    digest = _digest(manifest)
    generation = _digest({"manifest": digest, "host": host_binding(host), "network": network})[:16]
    base = "ebox-template-" + generation
    guest, resources = host.guest, manifest["recommended"]
    contribution = {key: getattr(host, key) for key in ("contribution_cpu", "contribution_memory_gib")
                    if getattr(host, key, None) is not None}
    return normalize({"schema_version": 1,
                      "host": {"transport": "local" if host.target == "local" else "ssh", "target": host.target,
                               "install_packages": False, "disk_reserve": f"{max(1, host.reserve_disk_gib)}GiB",
                               "memory_reserve": f"{max(1, host.reserve_memory_gib)}GiB",
                               "admission": {"reserve_cpu": host.reserve_cpu,
                                             "reserve_memory_gib": host.reserve_memory_gib,
                                             "reserve_disk_gib": host.reserve_disk_gib,
                                             "max_instances": host.max_instances, **contribution}},
                      "incus": {"project": host.project, "profile": base},
                      "storage": {"mode": "existing", "pool": host.pool},
                      "network": {"mode": "existing", "name": network},
                      "workspace": {"name": base, "image": manifest["source_image"], "cpu": resources["cpu"],
                                    "memory": f"{resources['memory_gib']}GiB", "disk": f"{manifest['minimum']['disk_gib']}GiB",
                                    "state_disk": f"{resources['memory_gib'] + 2}GiB", "user": guest.user,
                                    "uid": guest.uid, "gid": guest.gid, "home": guest.home, "path": guest.workspace},
                      "agents": {"install": manifest["native_clients"]},
                      "modules": manifest["config"]})


# Finalization reacquires the same host lock as bootstrap/fleet and verifies the
# exact newly prepared base before stopping it. No login/user VM is a candidate.
FINALIZE_SCRIPT = r'''
import fcntl, json, os, stat, subprocess, sys
from urllib.parse import quote
c=json.loads(sys.argv[1]); p=c['project']; n=c['source']; s=c['snapshot']
fd=os.open('/run/lock/enginebox-host.lock',os.O_CREAT|os.O_RDWR|os.O_NOFOLLOW,0o600)
st=os.fstat(fd)
if st.st_uid!=0 or st.st_mode & 0o022 or not stat.S_ISREG(st.st_mode): raise SystemExit('Unsafe host lock')
fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
def query(path): return json.loads(subprocess.check_output(['incus','query',path],timeout=30))
suffix='?project='+quote(p,safe='')
vm=query('/1.0/instances/'+quote(n,safe='')+suffix)
cfg=vm.get('config',{})
if (cfg.get('user.enginebox.owner')!=p or cfg.get('user.enginebox.recipe')!=c['fingerprint']
    or cfg.get('user.enginebox.prepared')!=c['fingerprint'] or vm.get('profiles')!=[n]
    or vm.get('type')!='virtual-machine'): raise SystemExit('Prepared template ownership/configuration mismatch')
snap=query('/1.0/instances/'+quote(n,safe='')+'/snapshots/'+quote(s,safe='')+suffix)
if snap.get('config',{}).get('user.enginebox.clean-base')!='true' or snap.get('stateful',False):
    raise SystemExit('Prepared template snapshot is not a clean disk-only base')
if vm['status']=='Running':
    subprocess.run(['incus','--project',p,'stop',n,'--timeout=120'],check=True,timeout=150)
elif vm['status']!='Stopped' or vm.get('stateful',False): raise SystemExit('Template base is in an unexpected state')
print(json.dumps({'source':n,'snapshot':s,'snapshot_created_at':snap.get('created_at')}))
'''


def install_template(host, manifest):
    """Installer adapter; bootstrap owns its host flock, so callers must not hold it."""
    manifest = validate_manifest(manifest)
    network = discover_network(host)
    config = bootstrap_configuration(host, manifest, network)
    observed = bootstrap.inventory(config)
    plan = bootstrap.build_plan(config, observed)
    if not plan["ok"]:
        raise ValueError("Template installation preflight failed: " + "; ".join(plan["errors"]))
    allowed = {"create_profile", "create_workspace", "prepare_workspace"}
    if any(operation["action"] not in allowed for operation in plan["operations"]):
        raise ValueError("Template installation requires an existing owned project, storage pool and managed bridge; bootstrap the host first.")
    result = bootstrap.apply(config)
    if not result.get("ok"):
        raise RuntimeError("Template bootstrap did not report successful preparation.")
    source = config["workspace"]["name"]
    snapshot = "prepared-" + fingerprint(config)[:12]
    expected = {"project": host.project, "source": source, "snapshot": snapshot, "fingerprint": fingerprint(config)}
    final = json.loads(bootstrap.run_command(bootstrap.transport_command(config,
                       ["python3", "-c", FINALIZE_SCRIPT, json.dumps(expected)]), timeout=180))
    if final.get("source") != source or final.get("snapshot") != snapshot:
        raise RuntimeError("Template finalization returned an unexpected source or snapshot.")
    return final | {"network": network, "bootstrap_fingerprint": fingerprint(config)}


class Registry:
    def __init__(self, state_dir):
        self.root = Path(state_dir).absolute() / "templates"
        self._mutex = threading.RLock()
        self._directory(self.root)
        for child in ["manifests", "history", "deployments", "locks"]:
            self._directory(self.root / child)

    @staticmethod
    def _directory(path):
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        info = path.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError("Template registry directories must be private, owned directories, not symlinks.")

    @contextmanager
    def _lock(self, name="registry", *, blocking=True):
        path = self.root / "locks" / (name + ".lock")
        fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise ValueError("Unsafe template registry lock.")
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
            except BlockingIOError as exc:
                raise ValueError("This template installation is already running for the host.") from exc
            yield
        finally:
            os.close(fd)

    @staticmethod
    def _read(path):
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd, "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise ValueError("Unsafe template registry record.")
            return _decode(stream.read(MAX_MANIFEST_BYTES + 1))

    @staticmethod
    def _write(path, value):
        temporary = path.with_name("." + uuid.uuid4().hex + ".tmp")
        try:
            fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, "wb") as stream:
                stream.write(_canonical(value))
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, path)
            directory = os.open(path.parent, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            temporary.unlink(missing_ok=True)

    def register(self, manifest):
        manifest = validate_manifest(manifest)
        digest = _digest(manifest)
        with self._mutex, self._lock():
            path = self.root / "manifests" / (manifest["id"] + ".json")
            if path.exists():
                current = validate_manifest(self._read(path))
                if current != manifest and manifest["revision"] <= current["revision"]:
                    raise ValueError("Changed templates require a revision greater than the registered revision.")
            history = self.root / "history" / f"{manifest['id']}-{manifest['revision']}-{digest}.json"
            self._write(history, manifest)
            self._write(path, manifest)
        return self.get(manifest["id"])

    def import_url(self, url):
        return self.register(fetch_manifest(url))

    def get(self, template_id):
        _id(template_id)
        try:
            manifest = validate_manifest(self._read(self.root / "manifests" / (template_id + ".json")))
        except FileNotFoundError as exc:
            raise ValueError(f"Unknown template: {template_id}") from exc
        if manifest["id"] != template_id:
            raise ValueError("Template record ID does not match its filename.")
        digest = _digest(manifest)
        public = []
        for receipt in self.deployments():
            if receipt["id"] == template_id:
                # Host connection aliases stay in the private receipt, not the catalog.
                public.append({key: value for key, value in receipt.items() if key not in {"target", "host_binding"}} |
                              {"in_sync": receipt["manifest_digest"] == digest})
        return manifest | {"manifest_digest": digest, "deployments": public}

    def list(self):
        return [self.get(path.stem) for path in sorted((self.root / "manifests").glob("*.json"))]

    def deployments(self, host_id=None):
        receipts = []
        for path in sorted((self.root / "deployments").glob("*.json")):
            receipt = self._read(path)
            required = {"id", "host_id", "manifest_digest", "host_binding_digest", "host_binding", "source", "snapshot"}
            if not required <= set(receipt) or not isinstance(receipt["host_id"], str):
                raise ValueError("Invalid template deployment receipt.")
            _id(receipt["id"])
            if path.stem != _digest([receipt["host_id"], receipt["id"]]):
                raise ValueError("Deployment receipt does not match its filename.")
            if receipt["host_binding_digest"] != _digest(receipt["host_binding"]):
                raise ValueError("Deployment receipt host binding is inconsistent.")
            if host_id is None or receipt["host_id"] == host_id:
                receipts.append(receipt)
        return receipts

    def install(self, host, template_id):
        record = self.get(template_id)
        manifest = {key: value for key, value in record.items() if key in MANIFEST_KEYS}
        binding = host_binding(host)
        # Releasing the registry-wide lock during provisioning allows import of
        # a newer revision. Its deployment then correctly appears out of sync.
        lock_name = "install-" + _digest([host.id, binding, template_id])
        with self._lock(lock_name, blocking=False):
            result = install_template(host, manifest)
            receipt = {"schema_version": 1, "id": template_id, "label": manifest["label"],
                       "revision": manifest["revision"], "manifest_digest": record["manifest_digest"],
                       "minimum": manifest["minimum"], "recommended": manifest["recommended"],
                       "host_id": host.id, "target": host.target, "project": host.project, "pool": host.pool,
                       "guest": binding["guest"], "host_binding": binding, "host_binding_digest": _digest(binding),
                       "installed_at": datetime.now(timezone.utc).isoformat(), **result}
            with self._mutex, self._lock():
                self._write(self.root / "deployments" / (_digest([host.id, template_id]) + ".json"), receipt)
            return receipt
