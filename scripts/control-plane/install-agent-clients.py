"""Install original provider CLIs in an owned controller directory, as its user."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile


RECIPES = {
    "codex": ("https://chatgpt.com/codex/install.sh", "sh", ["--release", "0.163.0"]),
    "claude": ("https://claude.ai/install.sh", "bash", ["2.1.302"]),
    "agent": ("https://cursor.com/install", "bash", []),
}


def directory(path):
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("Agent client storage must be a private directory owned by the controller user.")


def install(state_dir):
    if os.geteuid() == 0:
        raise ValueError("Run native client installation as the unprivileged controller service user.")
    root = state_dir / "agents"
    home = root / "client-install"
    bindir = root / "bin"
    for path in (state_dir, root, home, bindir):
        directory(path)
    marker = home / ".enginebox-agent-clients"
    marker_text = "Engine Box native controller clients v1\n"
    if marker.is_symlink() or (marker.exists() and (not marker.is_file() or marker.read_text() != marker_text)):
        raise ValueError("Native-client ownership marker is invalid.")
    if not marker.exists() and any(home.iterdir()):
        raise ValueError("Refusing to use an unmarked nonempty native-client directory.")
    marker.write_text(marker_text)
    marker.chmod(0o600)
    node = shutil.which("node")
    environment = {"HOME": str(home), "USER": os.environ.get("USER", ""),
                   "LOGNAME": os.environ.get("LOGNAME", ""), "SHELL": "/bin/bash",
                   "PATH": ":".join([str(home / ".local/bin"), str(Path(node).parent) if node else "/usr/local/bin",
                                     "/usr/local/bin", "/usr/bin", "/bin"]), "DISABLE_AUTOUPDATER": "1"}
    versions = {}
    for binary, (url, shell, arguments) in RECIPES.items():
        executable = home / ".local/bin" / binary
        if not executable.exists():
            with tempfile.TemporaryDirectory(prefix="install-", dir=home) as temporary:
                script = Path(temporary) / "installer"
                subprocess.run(["curl", "--fail", "--silent", "--show-error", "--location", "--proto", "=https",
                                "--proto-redir", "=https", "--max-time", "60", url, "--output", str(script)],
                               check=True, env=environment)
                subprocess.run([shell, str(script), *arguments], cwd=home, env=environment, check=True, timeout=300)
        version = subprocess.check_output([str(executable), "--version"], env=environment, timeout=20, text=True).strip()
        link = bindir / binary
        if link.is_symlink():
            if link.resolve() != executable.resolve():
                raise ValueError("Existing native-client link points outside its owned installation.")
        elif link.exists():
            raise ValueError("Refusing to replace an existing native-client binary.")
        else:
            link.symlink_to(executable)
        versions[binary] = version
    manifest = root / "installed-versions.json"
    if manifest.is_symlink() or (manifest.exists() and not manifest.is_file()):
        raise ValueError("Native-client version manifest must be a regular file.")
    manifest.write_text(json.dumps(versions, indent=2) + "\n")
    manifest.chmod(0o600)
    print(json.dumps({"installed": versions, "location": str(bindir)}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, required=True)
    args = parser.parse_args()
    install(args.state_dir.absolute())
