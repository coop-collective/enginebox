"""Install a built control plane on an existing Linux/systemd host.

Never provisions compute, changes firewall/sudoers, installs Node, or emits
credential values. A complete read-only preflight precedes every application.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import grp
import os
from pathlib import Path
import pwd
import re
import shlex
import shutil
import socket
import subprocess
import sys
import uuid

MARKER = "Managed by Engine Box control-plane installer v1"
UNITS = ("enginebox-fleet-api.service", "enginebox-dashboard.service")
ENV_KEYS = {"HOST", "PORT", "ENGINEBOX_API_URL", "ENGINEBOX_API_TOKEN",
            "ENGINEBOX_DASHBOARD_ORIGIN", "ENGINEBOX_DASHBOARD_PASSWORD", "ENGINEBOX_SESSION_SECRET"}


def safe_path(path: Path) -> Path:
    value = str(path.absolute())
    if not re.fullmatch(r"/[a-zA-Z0-9_./-]+", value) or ".." in Path(value).parts:
        raise ValueError("Paths must be absolute and contain only letters, digits, slash, dot, underscore or hyphen.")
    return Path(value)


def private_file(path: Path):
    if path.is_symlink() or not path.is_file() or path.stat().st_mode & 0o077:
        raise ValueError(f"Private input must be a regular non-symlink file with no group/other permissions: {path}")


def read_environment(path: Path) -> dict[str, str]:
    result = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        key, separator, value = line.partition("=")
        if not separator or key not in ENV_KEYS or key in result:
            raise ValueError("Environment file contains an unknown, duplicate or malformed setting; credential contents are omitted.")
        try:
            parts = shlex.split(value, comments=True)
        except ValueError:
            raise ValueError("Malformed environment value; credential contents are omitted.") from None
        if len(parts) != 1 or any(ord(char) < 32 or ord(char) == 127 for char in parts[0]):
            raise ValueError("Environment values must be one nonempty literal value without control characters.")
        result[key] = parts[0]
    return result


def validate_environment(values: dict[str, str], api_port: int) -> dict[str, str]:
    for key, length in [("ENGINEBOX_API_TOKEN", 32), ("ENGINEBOX_DASHBOARD_PASSWORD", 16), ("ENGINEBOX_SESSION_SECRET", 32)]:
        if len(values.get(key, "")) < length:
            raise ValueError(f"{key} must contain at least {length} characters.")
    if values["ENGINEBOX_API_TOKEN"] == values["ENGINEBOX_DASHBOARD_PASSWORD"]:
        raise ValueError("Use distinct API and dashboard credentials.")
    if values.get("HOST", "127.0.0.1") != "127.0.0.1":
        raise ValueError("This installer binds both services to 127.0.0.1 only.")
    try:
        port = int(values.get("PORT", "3000"))
    except ValueError:
        raise ValueError("PORT must be a numeric TCP port.") from None
    if not 1024 <= port <= 65535 or not 1024 <= api_port <= 65535 or port == api_port:
        raise ValueError("Use different unprivileged API/dashboard ports from 1024 to 65535.")
    api_url = f"http://127.0.0.1:{api_port}"
    if values.get("ENGINEBOX_API_URL", api_url) != api_url:
        raise ValueError("ENGINEBOX_API_URL must match this installation's loopback API port.")
    origin = values.get("ENGINEBOX_DASHBOARD_ORIGIN", f"http://127.0.0.1:{port}")
    # HTTPS is permitted for an operator-managed TLS reverse proxy, while the
    # actual service still binds loopback. No forwarded headers are trusted.
    from urllib.parse import urlsplit
    parsed = urlsplit(origin)
    if parsed.username or parsed.password or parsed.path or parsed.query or parsed.fragment or not parsed.hostname:
        raise ValueError("ENGINEBOX_DASHBOARD_ORIGIN must be an exact origin without credentials or a path.")
    if origin != f"http://127.0.0.1:{port}" and parsed.scheme != "https":
        raise ValueError("Use the matching loopback origin or an HTTPS reverse-proxy origin.")
    return {**values, "HOST": "127.0.0.1", "PORT": str(port),
            "ENGINEBOX_API_URL": api_url, "ENGINEBOX_DASHBOARD_ORIGIN": origin}


def write_private(path: Path, contents: str, user):
    path.write_text(contents)
    path.chmod(0o600)
    os.chown(path, user.pw_uid, user.pw_gid)


def environment_text(values: dict[str, str]) -> str:
    # systemd EnvironmentFile accepts double-quoted literal values. Do not
    # source the operator's input as a shell script.
    return "".join(f'{key}="{value.replace(chr(92), chr(92) * 2).replace(chr(34), chr(92) + chr(34))}"\n'
                   for key, value in sorted(values.items()))


def check_venv_seed():
    # Importing ensurepip alone is insufficient on split distro installs: the
    # module can exist while its separately packaged pip wheel is absent.
    # Inspect the seed archive without bootstrapping pip or creating a venv.
    probe = """import ensurepip, pathlib, sysconfig, venv, zipfile
directories = [pathlib.Path(ensurepip.__file__).parent / '_bundled']
distro_wheels = sysconfig.get_config_var('WHEEL_PKG_DIR')
if distro_wheels:
    directories.insert(0, pathlib.Path(distro_wheels))
wheels = [wheel for directory in directories if directory.is_dir()
          for wheel in directory.glob('pip-*.whl') if wheel.is_file()]
if not wheels:
    raise RuntimeError('No pip seed wheel is available')
with zipfile.ZipFile(wheels[0]) as archive:
    archive.read('pip/__main__.py')
    archive.read('pip/__init__.py')
"""
    result = subprocess.run([sys.executable, "-I", "-B", "-c", probe], capture_output=True)
    if result.returncode:
        version = f"{sys.version_info.major}.{sys.version_info.minor}"
        raise ValueError(
            f"Python {version} cannot seed pip into a venv. Install python3-venv "
            f"(or python{version}-venv for this interpreter) through the OS package manager, "
            "then rerun the plan. No host packages were installed."
        )


def preflight(args):
    if sys.platform != "linux" or not Path("/run/systemd/system").is_dir():
        raise ValueError("Install on a Linux host running systemd; local source validation does not need installation.")
    if sys.version_info < (3, 11):
        raise ValueError("Python 3.11 or newer is required.")
    check_venv_seed()
    if not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", args.user):
        raise ValueError("--user must be an existing Linux username.")
    user = pwd.getpwnam(args.user)
    if user.pw_uid == 0:
        raise ValueError("Choose a non-root service user with existing SSH/Incus access.")
    safe_path(Path(user.pw_dir))
    if not Path(user.pw_dir).is_dir():
        raise ValueError("The service user needs an existing home directory for its SSH/Incus configuration.")
    group = grp.getgrgid(user.pw_gid).gr_name
    if not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", group):
        raise ValueError("Service group name cannot be represented safely in the units.")
    for path in [args.config, args.env_file]:
        private_file(path)
    if args.prefix.is_symlink() or args.prefix == Path("/"):
        raise ValueError("Installation prefix cannot be a symlink or filesystem root.")
    marker = args.prefix / ".enginebox-control"
    if args.prefix.exists() and (marker.is_symlink() or not marker.is_file() or marker.read_text().strip() != MARKER):
        raise ValueError("Existing installation prefix is not owned by this installer; choose an unused prefix.")
    for name in ["state", "releases"]:
        path = args.prefix / name
        if path.is_symlink() or (path.exists() and not path.is_dir()):
            raise ValueError(f"Unexpected {name} entry in owned installation prefix.")
    for name in ["current", "previous"]:
        path = args.prefix / name
        if path.is_symlink():
            if not path.resolve().is_relative_to((args.prefix / "releases").resolve()) or not path.is_dir():
                raise ValueError(f"Unexpected {name} target in owned installation prefix.")
        elif path.exists():
            raise ValueError(f"Unexpected non-symlink {name} entry in owned installation prefix.")
    if (args.prefix / ".current-next").exists() or (args.prefix / ".current-next").is_symlink():
        raise ValueError("A pending installation link already exists; inspect the previous installation attempt.")
    for name in UNITS:
        unit = Path("/etc/systemd/system") / name
        if unit.is_symlink() or (unit.exists() and
            (f"# {MARKER}\n" not in unit.read_text() or f"# Engine Box prefix: {args.prefix}\n" not in unit.read_text())):
            raise ValueError(f"Refusing to overwrite an unknown or differently located unit: {name}")
        fragment = subprocess.run(["systemctl", "show", "--property=FragmentPath", "--value", name], capture_output=True, text=True).stdout.strip()
        if fragment and fragment != str(unit):
            raise ValueError(f"Refusing to override a unit from another systemd directory: {name}")
    node = shutil.which("node")
    if not node or not shutil.which("systemctl"):
        raise ValueError("Existing Node.js and systemctl are required; the installer does not install them.")
    if not args.skip_agent_clients and any(not shutil.which(binary) for binary in ("curl", "bash", "runuser")):
        raise ValueError("Native controller client installation requires existing curl, bash and runuser.")
    node = str(safe_path(Path(node).resolve()))
    version = subprocess.check_output([node, "--version"], text=True).strip().lstrip("v")
    if tuple(map(int, version.split(".")[:2])) < (22, 12):
        raise ValueError("Existing Node.js must be version 22.12 or newer.")
    runtime = args.checkout / "runtime"
    artifact = args.checkout / "dashboard/dist/standalone"
    for path in [runtime / "pyproject.toml", runtime / "requirements.lock", runtime / "fleet_api.py",
                 artifact / "dist/server/index.js", artifact / "node_modules/vinext/package.json",
                 args.checkout / "dashboard/scripts/start.mjs", args.checkout / "scripts/control-plane/install-agent-clients.py"]:
        if not path.is_file():
            raise ValueError(f"Missing source or built artifact: {path}. Build the dashboard on the target platform first.")
    env = validate_environment(read_environment(args.env_file), args.api_port)
    # Validate TOML with the same code the service uses, without contacting hosts.
    validation_env = {**os.environ, "PYTHONPATH": str(runtime), "PYTHONDONTWRITEBYTECODE": "1", "ENGINEBOX_GUEST": "0"}
    result = subprocess.run([sys.executable, "-c", "from fleet import load_hosts; import sys; load_hosts(sys.argv[1])", str(args.config)],
                            env=validation_env, capture_output=True)
    if result.returncode:
        raise ValueError("Fleet configuration validation failed; inspect it using the runtime. Its contents are not printed.")
    for port, name in [(args.api_port, UNITS[0]), (int(env["PORT"]), UNITS[1])]:
        with socket.socket() as sock:
            try:
                sock.bind(("127.0.0.1", port))
            except OSError:
                owned = Path("/etc/systemd/system", name).exists()
                if not owned or subprocess.run(["systemctl", "is-active", "--quiet", name]).returncode:
                    raise ValueError(f"Loopback port {port} is already occupied by an unrelated service.") from None
    return user, group, node, env


def apply(args, user, group, node, values):
    if os.geteuid() != 0:
        raise ValueError("--apply requires root; the default plan does not change the host.")
    os.umask(0o022)
    prefix = args.prefix
    prefix.mkdir(parents=True, exist_ok=True, mode=0o755)
    (prefix / ".enginebox-control").write_text(MARKER + "\n")
    release = prefix / "releases" / (datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8])
    release.mkdir(parents=True, mode=0o755)
    shutil.copytree(args.checkout / "runtime", release / "runtime",
                    ignore=shutil.ignore_patterns(".venv", "__pycache__", "*.egg-info", ".env*", ".state", ".enginebox", "*.local.toml", "build", "dist"))
    shutil.copytree(args.checkout / "dashboard/dist/standalone", release / "dashboard")
    (release / "dashboard/scripts").mkdir()
    shutil.copy2(args.checkout / "dashboard/scripts/start.mjs", release / "dashboard/scripts/start.mjs")
    subprocess.run([sys.executable, "-m", "venv", str(release / "venv")], check=True)
    pip = [str(release / "venv/bin/python"), "-m", "pip", "--disable-pip-version-check", "--no-input"]
    subprocess.run([*pip, "install", "--require-hashes", "-r", str(release / "runtime/requirements.lock")], check=True)
    subprocess.run([*pip, "install", "--no-deps", str(release / "runtime")], check=True)
    # A source checkout built with umask 077 must still produce code readable
    # by the service user. Never follow venv symlinks and chmod host binaries.
    for path in release.rglob("*"):
        if not path.is_symlink():
            path.chmod(0o755 if path.is_dir() or path.stat().st_mode & 0o111 else 0o644)
    # No credentials are passed in process argv, logs or unit files.
    private = release / "etc"
    private.mkdir(mode=0o700)
    os.chown(private, user.pw_uid, user.pw_gid)
    write_private(private / "fleet.toml", args.config.read_text(), user)
    write_private(private / "api.env", environment_text({"ENGINEBOX_API_TOKEN": values["ENGINEBOX_API_TOKEN"], "ENGINEBOX_API_PORT": str(args.api_port)}), user)
    write_private(private / "dashboard.env", environment_text(values), user)
    state = prefix / "state"
    state.mkdir(exist_ok=True, mode=0o700)
    os.chown(state, user.pw_uid, user.pw_gid)
    if not args.skip_agent_clients:
        subprocess.run(["runuser", "-u", user.pw_name, "--", sys.executable,
                        str(Path(__file__).parent / "install-agent-clients.py"),
                        "--state-dir", str(state)], check=True)
    substitutions = {"PREFIX": str(prefix), "USER": user.pw_name, "GROUP": group, "HOME": user.pw_dir, "NODE": node}
    unit_texts = {}
    for name in UNITS:
        text = (Path(__file__).parent / (name + ".in")).read_text()
        for key, value in substitutions.items():
            text = text.replace("@" + key + "@", value)
        unit_texts[name] = text
    current = prefix / "current"
    previous = prefix / "previous"
    if current.is_symlink():
        if previous.is_symlink():
            previous.unlink()
        elif previous.exists():
            raise ValueError("Unexpected previous entry in owned installation prefix.")
        previous.symlink_to(os.readlink(current))
    elif current.exists():
        raise ValueError("Unexpected non-symlink current entry in owned installation prefix.")
    pending = prefix / ".current-next"
    pending.symlink_to(release.relative_to(prefix))
    pending.replace(current)
    for name, text in unit_texts.items():
        destination = Path("/etc/systemd/system") / name
        destination.write_text(text)
        destination.chmod(0o644)
    subprocess.run(["systemctl", "daemon-reload"], check=True)
    subprocess.run(["systemctl", "enable", *UNITS], check=True)
    subprocess.run(["systemctl", "restart", *UNITS], check=True)
    print("Installed owned loopback services. Check both health endpoints and systemctl status; credentials were not printed.")
    print(f"Current release: {release}. Earlier releases and shared job state are retained for rollback.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--env-file", required=True, type=Path)
    parser.add_argument("--user", required=True)
    parser.add_argument("--checkout", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--prefix", type=Path, default=Path("/opt/enginebox-control"))
    parser.add_argument("--api-port", type=int, default=8765)
    parser.add_argument("--apply", action="store_true", help="Apply the printed plan after successful preflight.")
    parser.add_argument("--skip-agent-clients", action="store_true", help="Leave native controller clients uninstalled (Agents login will be unavailable).")
    args = parser.parse_args()
    try:
        for field in ["config", "env_file", "checkout", "prefix"]:
            setattr(args, field, safe_path(getattr(args, field)))
        user, group, node, values = preflight(args)
        print("Preflight passed. Plan:")
        print(f"  Install an isolated runtime venv and built Vinext release under {args.prefix}.")
        print(f"  Copy private inputs with owner-only permissions; run as existing user {user.pw_name}.")
        print(f"  Use existing Node {node}; API 127.0.0.1:{args.api_port}, dashboard 127.0.0.1:{values['PORT']}.")
        print("  Install/restart only enginebox-fleet-api.service and enginebox-dashboard.service.")
        if not args.skip_agent_clients:
            print("  Prepare original Codex, Claude and Cursor CLIs in private controller storage for account connections.")
        print("  Preserve existing host software, firewall, sudoers, compute resources and earlier releases.")
        if args.apply:
            apply(args, user, group, node, values)
        else:
            print("Plan only; no changes made. Repeat with --apply to install.")
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Engine Box control-plane install: {error}\n")


if __name__ == "__main__":
    main()
