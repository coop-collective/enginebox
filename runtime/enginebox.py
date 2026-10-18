"""Thin environment controls over SSH and Incus. No agent harness adapters."""
from __future__ import annotations

import argparse
from dataclasses import dataclass
import ipaddress
import json
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path, PurePosixPath
from urllib.parse import urlencode


def guest_mode() -> bool:
    value = os.environ.get("ENGINEBOX_GUEST", "0")
    if value not in {"0", "1"}:
        raise ValueError("ENGINEBOX_GUEST must be '0' (operator) or '1' (guest desktop only).")
    return value == "1"


def configured_id(variable: str) -> int:
    value = os.environ.get(variable, "1001")
    if not re.fullmatch(r"[0-9]{1,10}", value) or not 1 <= int(value) <= 4294967294:
        raise ValueError(f"{variable} must be a non-root numeric UID/GID (1–4294967294).")
    return int(value)


def configured_path(variable: str, default: str) -> str:
    value = os.environ.get(variable, default)
    if (not value.startswith("/") or ".." in PurePosixPath(value).parts
            or any(ord(char) < 32 or ord(char) == 127 for char in value)):
        raise ValueError(f"{variable} must be an absolute guest path without '..' or control characters.")
    return value


@dataclass(frozen=True)
class GuestConfig:
    uid: int
    gid: int
    user: str
    home: str
    workspace: str
    display: str

    @classmethod
    def from_env(cls) -> GuestConfig:
        user = os.environ.get("ENGINEBOX_GUEST_USER", "agent")
        if not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", user) or user == "root":
            raise ValueError("ENGINEBOX_GUEST_USER must be a non-root Linux username.")
        display = os.environ.get("ENGINEBOX_GUEST_DISPLAY", ":99")
        if not re.fullmatch(r":[0-9]{1,5}(?:\.[0-9]{1,5})?", display):
            raise ValueError("ENGINEBOX_GUEST_DISPLAY must be a local X11 display, for example ':99'.")
        return cls(configured_id("ENGINEBOX_GUEST_UID"), configured_id("ENGINEBOX_GUEST_GID"),
                   user, configured_path("ENGINEBOX_GUEST_HOME", "/home/agent"),
                   configured_path("ENGINEBOX_GUEST_WORKSPACE", "/workspace"), display)


def identifier(value: str, *, instance: bool = False) -> str:
    pattern = r"ebox-[a-z0-9][a-z0-9-]{0,48}" if instance else r"[a-z0-9][a-z0-9-]{0,48}"
    if not re.fullmatch(pattern, value):
        raise ValueError("Use ebox- prefixed instance names and lowercase letters, digits or hyphens.")
    return value


class EngineBox:
    def __init__(self, host: str | None = None):
        if guest_mode():
            raise ValueError("Host environment controls are unavailable in ENGINEBOX_GUEST=1 mode.")
        self.host = host if host is not None else os.environ.get("ENGINEBOX_HOST", "")
        if not self.host:
            raise ValueError("Set ENGINEBOX_HOST explicitly to an SSH host/alias or 'local'.")
        if not re.fullmatch(r"(?:[a-zA-Z0-9_][a-zA-Z0-9_.-]*@)?[a-zA-Z0-9][a-zA-Z0-9_.-]*", self.host):
            raise ValueError("ENGINEBOX_HOST must be an SSH host/alias, optionally user@host, or 'local'.")
        self.project = os.environ.get("ENGINEBOX_PROJECT", "enginebox")
        if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_-]{0,62}", self.project):
            raise ValueError("ENGINEBOX_PROJECT must contain 1–63 letters, digits, underscores or hyphens and start with a letter or digit.")
        self.guest = GuestConfig.from_env()

    def command(self, *args: str, interactive: bool = False) -> list[str]:
        prefix = ["sudo", "-n", "incus"]
        command = prefix + ([] if args[:1] == ("query",) else ["--project", self.project]) + list(args)
        if self.host == "local":
            return command
        return ["ssh", "-tt" if interactive else "-T", "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=10", self.host, shlex.join(command)]

    def run(self, *args: str, timeout: int = 300) -> bytes:
        result = subprocess.run(self.command(*args), stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError(result.stderr.decode(errors="replace").strip() or
                               result.stdout.decode(errors="replace").strip())
        return result.stdout

    def list(self) -> list[dict]:
        return [{"name": vm["name"], "status": vm["status"],
                 "stateful": vm.get("stateful", False),
                 "ipv4": [a["address"] for n in ((vm.get("state") or {}).get("network") or {}).values()
                          for a in n.get("addresses", []) if a["family"] == "inet" and a["scope"] == "global"]}
                for vm in json.loads(self.run("list", "--format=json"))
                if vm["name"].startswith("ebox-")]

    def status(self, name: str) -> dict:
        identifier(name, instance=True)
        # Metadata does not ask a paused/hibernated guest agent for interfaces.
        vm = json.loads(self.run("query", f"/1.0/instances/{name}?{urlencode({'project': self.project})}"))
        return {"name": vm["name"], "status": vm["status"], "stateful": vm.get("stateful", False)}

    def lifecycle(self, name: str, action: str) -> dict:
        state = self.status(name)["status"]
        expected = {"pause": {"Running"}, "resume": {"Frozen", "Stopped"},
                    "hibernate": {"Running"}, "stop": {"Running"}}
        if action not in expected or state not in expected[action]:
            raise ValueError(f"Cannot {action} instance in {state}; resume a frozen VM before stopping it.")
        args = {"pause": ["pause", name], "resume": ["start", name],
                "hibernate": ["stop", name, "--stateful"], "stop": ["stop", name, "--timeout=60"]}
        self.run(*args[action])
        return self.status(name)

    def exec_args(self, name: str, argv: list[str]) -> list[str]:
        identifier(name, instance=True)
        if not argv or any("\0" in arg for arg in argv):
            raise ValueError("Provide a non-empty command argv without NUL bytes.")
        return ["exec", name, f"--user={self.guest.uid}", f"--group={self.guest.gid}",
                f"--cwd={self.guest.workspace}", f"--env=HOME={self.guest.home}",
                f"--env=USER={self.guest.user}", f"--env=DISPLAY={self.guest.display}",
                "--", "bash", "-lc", "exec -- " + shlex.join(argv)]

    def execute(self, name: str, argv: list[str], timeout: int = 60) -> bytes:
        if not 1 <= timeout <= 300:
            raise ValueError("Timeout must be 1–300 seconds. Use tmux for long-lived work.")
        return self.run(*self.exec_args(name, argv), timeout=timeout)

    def snapshot(self, name: str, snapshot: str) -> str:
        state = self.status(name)["status"]
        identifier(snapshot)
        if state == "Frozen":
            raise ValueError("Resume before taking a snapshot.")
        args = ["snapshot", "create", name, snapshot]
        if state == "Running":
            args.append("--stateful")
        elif state != "Stopped" or self.status(name)["stateful"]:
            raise ValueError("Use a running VM or a cleanly stopped VM for a snapshot.")
        self.run(*args)
        return f"{name}/{snapshot}"

    def fork(self, source: str, snapshot: str, target: str) -> dict:
        identifier(source, instance=True)
        identifier(snapshot)
        identifier(target, instance=True)
        self.run("copy", f"{source}/{snapshot}", target, "--stateless")
        return self.status(target)

    def restore(self, name: str, snapshot: str, stateful: bool = True) -> dict:
        if self.status(name)["status"] != "Stopped":
            raise ValueError("Stop the VM explicitly before restoring its disk and memory.")
        identifier(snapshot)
        args = ["snapshot", "restore", name, snapshot]
        if stateful:
            args.append("--stateful")
        self.run(*args)
        return self.status(name)

    def desktop(self, name: str) -> dict:
        identifier(name, instance=True)
        address = next(vm for vm in self.list() if vm["name"] == name)["ipv4"][0]
        address = str(ipaddress.IPv4Address(address))
        if self.host == "local":
            return {"tunnel": None, "tunnel_argv": None,
                    "url": f"http://{address}:6080/vnc.html?autoconnect=true"}
        argv = ["ssh", "-N", "-o", "ExitOnForwardFailure=yes", "-L",
                f"127.0.0.1:16080:{address}:6080", self.host]
        return {"tunnel": shlex.join(argv), "tunnel_argv": argv,
                "url": "http://127.0.0.1:16080/vnc.html?autoconnect=true"}


def computer(argv: list[str], name: str = "ebox-dev") -> bytes:
    if guest_mode():
        result = subprocess.run(argv, env={**os.environ, "DISPLAY": GuestConfig.from_env().display},
                                capture_output=True, timeout=30, check=True)
        return result.stdout
    return EngineBox().execute(name, argv, timeout=30)


def screenshot(name: str = "ebox-dev") -> bytes:
    # scrot reopens /dev/stdout, which Incus owns as root. A private temporary
    # file also avoids collisions between concurrent MCP screenshot requests.
    script = """import pathlib, subprocess, sys, tempfile
with tempfile.TemporaryDirectory() as directory:
    path = pathlib.Path(directory) / 'desktop.png'
    subprocess.run(['scrot', str(path)], check=True)
    sys.stdout.buffer.write(path.read_bytes())
"""
    return computer(["python3", "-c", script], name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subs = parser.add_subparsers(dest="action", required=True)
    subs.add_parser("list")
    for action in ["pause", "resume", "hibernate", "stop", "shell", "desktop"]:
        subs.add_parser(action).add_argument("name")
    for action in ["snapshot", "restore"]:
        p = subs.add_parser(action)
        p.add_argument("name")
        p.add_argument("snapshot")
        if action == "restore":
            p.add_argument("--disk-only", action="store_true")
    p = subs.add_parser("fork")
    p.add_argument("name")
    p.add_argument("snapshot")
    p.add_argument("target")
    p = subs.add_parser("exec")
    p.add_argument("name")
    p.add_argument("argv", nargs=argparse.REMAINDER)
    p = subs.add_parser("screenshot")
    p.add_argument("name")
    p.add_argument("output", type=Path)
    args = parser.parse_args()
    try:
        box = EngineBox()
        if args.action == "list":
            result = box.list()
        elif args.action in {"pause", "resume", "hibernate", "stop"}:
            result = box.lifecycle(args.name, args.action)
        elif args.action == "shell":
            sys.exit(subprocess.call(box.command(*box.exec_args(args.name, ["bash", "-l"]), interactive=True)))
        elif args.action == "exec":
            argv = args.argv[1:] if args.argv[:1] == ["--"] else args.argv
            sys.stdout.buffer.write(box.execute(args.name, argv))
            return
        elif args.action == "snapshot":
            result = box.snapshot(args.name, args.snapshot)
        elif args.action == "restore":
            result = box.restore(args.name, args.snapshot, not args.disk_only)
        elif args.action == "fork":
            result = box.fork(args.name, args.snapshot, args.target)
        elif args.action == "screenshot":
            args.output.write_bytes(screenshot(args.name))
            result = str(args.output)
        else:
            result = box.desktop(args.name)
        print(json.dumps(result, indent=2))
    except (ValueError, RuntimeError, StopIteration, IndexError, OSError, subprocess.TimeoutExpired) as exc:
        parser.exit(1, f"Engine Box: {exc or 'Instance or address not found'}\n")


if __name__ == "__main__":
    main()
