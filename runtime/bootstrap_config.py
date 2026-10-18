"""Versioned, secret-free bootstrap configuration (Python standard library only)."""
from __future__ import annotations

import hashlib
import ipaddress
import json
from pathlib import Path, PurePosixPath
import re
import tomllib


DEFAULTS = {
    "schema_version": 1,
    "host": {"transport": "ssh", "target": "", "data_dir": "/var/lib/enginebox",
             "install_packages": False, "disk_reserve": "5GiB", "memory_reserve": "1GiB"},
    "incus": {"project": "enginebox", "profile": "enginebox"},
    "storage": {"mode": "existing", "pool": "", "size": "120GiB"},
    "network": {"mode": "existing", "name": "", "subnet": "10.203.0.1/24"},
    "workspace": {"name": "ebox-dev", "image": "images:ubuntu/24.04/cloud", "cpu": 4,
                  "memory": "4GiB", "disk": "30GiB", "state_disk": "8GiB",
                  "user": "agent", "uid": 1001, "gid": 1001, "home": "/home/agent",
                  "path": "/workspace", "ready_timeout": 300},
    "agents": {"install": ["codex", "cursor", "claude"]},
    "modules": {"desktop": False, "browser": True, "operator_mcp": True,
                "guest_computer_mcp": False, "ci": False},
}


def size_bytes(value: str) -> int:
    if not isinstance(value, str) or not re.fullmatch(r"[1-9][0-9]*(?:MiB|GiB|TiB)", value):
        raise ValueError("Sizes must be positive integers with MiB, GiB or TiB suffixes.")
    match = re.fullmatch(r"([0-9]+)(MiB|GiB|TiB)", value)
    return int(match[1]) * {"MiB": 2**20, "GiB": 2**30, "TiB": 2**40}[match[2]]


def name(value: str, label: str) -> None:
    if not isinstance(value, str) or not re.fullmatch(r"[a-z][a-z0-9-]{0,48}", value):
        raise ValueError(f"{label} must be 1–49 lowercase letters, digits or hyphens, starting with a letter.")


def absolute_path(value: str, label: str) -> None:
    if (not isinstance(value, str) or not value.startswith("/") or value == "/"
            or ".." in PurePosixPath(value).parts
            or not re.fullmatch(r"/[a-zA-Z0-9_./-]+", value)):
        raise ValueError(f"{label} must be an absolute non-root path with ordinary path characters and no '..'.")


def normalize(raw: dict) -> dict:
    if not isinstance(raw, dict):
        raise ValueError("Configuration must be a TOML object.")
    unknown = set(raw) - set(DEFAULTS)
    if unknown:
        raise ValueError(f"Unknown or unsupported configuration sections: {', '.join(sorted(unknown))}")
    config = json.loads(json.dumps(DEFAULTS))
    if type(raw.get("schema_version", 1)) is not int or raw.get("schema_version", 1) != 1:
        raise ValueError("Only schema_version = 1 is supported.")
    for section, values in raw.items():
        if section == "schema_version":
            continue
        if not isinstance(values, dict):
            raise ValueError(f"{section} must be a table.")
        unknown = set(values) - set(DEFAULTS[section])
        if unknown:
            raise ValueError(f"Unknown {section} keys: {', '.join(sorted(unknown))}")
        for key, value in values.items():
            if type(value) is not type(DEFAULTS[section][key]):
                raise ValueError(f"{section}.{key} has the wrong type.")
        config[section].update(values)
    host = config["host"]
    if host["transport"] not in {"local", "ssh"}:
        raise ValueError("host.transport must be local or ssh.")
    if host["transport"] == "ssh" and (not isinstance(host["target"], str) or not re.fullmatch(
            r"(?:[a-zA-Z0-9_][a-zA-Z0-9_.-]*@)?[a-zA-Z0-9][a-zA-Z0-9_.-]*", host["target"])):
        raise ValueError("host.target must be an explicit SSH alias/hostname, optionally user@host.")
    absolute_path(host["data_dir"], "host.data_dir")
    for key in ["disk_reserve", "memory_reserve"]:
        size_bytes(host[key])
    for key in ["project", "profile"]:
        name(config["incus"][key], f"incus.{key}")
    storage, network, ws = config["storage"], config["network"], config["workspace"]
    name(storage["pool"], "storage.pool")
    name(network["name"], "network.name")
    if storage["mode"] not in {"existing", "development-loop"}:
        raise ValueError("storage.mode must be existing or development-loop.")
    size_bytes(storage["size"])
    if network["mode"] not in {"existing", "managed"}:
        raise ValueError("network.mode must be existing or managed.")
    try:
        interface = ipaddress.IPv4Interface(network["subnet"])
        private_blocks = [ipaddress.IPv4Network(s) for s in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]]
        if not any(interface.network.subnet_of(block) for block in private_blocks) or not 16 <= interface.network.prefixlen <= 28:
            raise ValueError("Use an RFC1918 IPv4 subnet with prefix /16 through /28.")
        if interface.ip in {interface.network.network_address, interface.network.broadcast_address}:
            raise ValueError("network.subnet must include a usable bridge address, e.g. 10.203.0.1/24.")
    except (ValueError, TypeError) as exc:
        raise ValueError(f"Invalid network.subnet: {exc}") from exc
    name(ws["name"], "workspace.name")
    if not ws["name"].startswith("ebox-"):
        raise ValueError("workspace.name must start with ebox-.")
    # Guest provisioning is qualified for these Ubuntu images only. Custom images
    # must be reviewed, rather than executing Ubuntu package commands blindly.
    if ws["image"] != "images:ubuntu/24.04/cloud":
        raise ValueError("workspace.image must be images:ubuntu/24.04/cloud; other guest releases are not qualified yet.")
    for key, low, high in [("cpu", 1, 512), ("uid", 1000, 60000), ("gid", 1000, 60000), ("ready_timeout", 30, 1800)]:
        if type(ws[key]) is not int or not low <= ws[key] <= high:
            raise ValueError(f"workspace.{key} must be an integer from {low} through {high}.")
    if not isinstance(ws["user"], str) or not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", ws["user"]) or ws["user"] == "root":
        raise ValueError("workspace.user must be a non-root Linux username.")
    for key in ["home", "path"]:
        absolute_path(ws[key], f"workspace.{key}")
    if not ws["home"].startswith("/home/") or ws["home"] == ws["path"]:
        raise ValueError("workspace.home must be under /home and distinct from workspace.path.")
    memory, disk, state = [size_bytes(ws[key]) for key in ["memory", "disk", "state_disk"]]
    if not 2**30 <= memory <= 512 * 2**30 or disk < 12 * 2**30:
        raise ValueError("Workspace needs 1–512 GiB RAM and at least 12 GiB disk.")
    if state < memory + 2**30:
        raise ValueError("workspace.state_disk must exceed configured RAM by at least 1GiB.")
    if storage["mode"] == "development-loop" and size_bytes(storage["size"]) < disk + state + size_bytes(host["disk_reserve"]):
        raise ValueError("storage.size must accommodate workspace disk, memory state and disk reserve.")
    if type(host["install_packages"]) is not bool:
        raise ValueError("host.install_packages must be boolean.")
    for key, value in config["modules"].items():
        if type(value) is not bool:
            raise ValueError(f"modules.{key} must be boolean.")
    if config["modules"]["ci"]:
        raise ValueError("The CI module is not implemented by bootstrap; leave modules.ci = false.")
    if config["modules"]["guest_computer_mcp"] and not config["modules"]["desktop"]:
        raise ValueError("Guest computer MCP requires modules.desktop = true.")
    clients = config["agents"]["install"]
    if not isinstance(clients, list) or any(not isinstance(x, str) or x not in {"codex", "cursor", "claude"} for x in clients) or len(set(clients)) != len(clients):
        raise ValueError("agents.install must be a unique list drawn from codex, cursor, claude.")
    return config


def load_config(path: str | Path) -> dict:
    with Path(path).open("rb") as stream:
        return normalize(tomllib.load(stream))


def fingerprint(config: dict) -> str:
    # Connection aliases and local journal location are not guest identity.
    desired = {key: value for key, value in config.items() if key != "host"}
    return hashlib.sha256(json.dumps(desired, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def runtime_environment(config: dict) -> dict[str, str]:
    ws = config["workspace"]
    return {"ENGINEBOX_HOST": config["host"]["target"] if config["host"]["transport"] == "ssh" else "local",
            "ENGINEBOX_PROJECT": config["incus"]["project"],
            "ENGINEBOX_GUEST_USER": ws["user"], "ENGINEBOX_GUEST_UID": str(ws["uid"]),
            "ENGINEBOX_GUEST_GID": str(ws["gid"]), "ENGINEBOX_GUEST_HOME": ws["home"],
            "ENGINEBOX_GUEST_WORKSPACE": ws["path"]}
