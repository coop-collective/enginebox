"""Single-owner controller connections and access-only native credential delivery.

Provider harnesses still connect directly to their providers. Refresh credentials
stay in the controller HOME. This module never returns credential contents from
its public methods or puts them in command arguments, job records or errors.
"""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from datetime import datetime, timezone
import base64
from dataclasses import asdict
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time

from accounts import AccountManager
from agent_usage import UsageCache, read_codex, unavailable as usage_unavailable
import guest_auth


MAX_CREDENTIAL_BYTES = 65536
PROVIDERS = tuple(guest_auth.PROVIDERS)
NATIVE_FILES = {"codex": ".codex/auth.json", "claude": ".claude/.credentials.json",
                "cursor": ".config/cursor/auth.json"}
TOKEN_FILE = ".config/enginebox/controller-token.json"
CODEX_VERSION = "0.163.0"
CURSOR_VERSION = "2026.10.16-e3cb207"


def provider_id(value):
    if not isinstance(value, str) or value not in PROVIDERS:
        raise ValueError("Unknown native agent provider.")
    return value


def private_directory(path):
    path = Path(path)
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
        raise ValueError("Agent credential directories must be private and owned by the controller user.")
    return path


def _check_path(path, boundary):
    path, boundary = Path(path), Path(boundary)
    path.relative_to(boundary)
    current = boundary
    if current.is_symlink():
        raise ValueError("Agent credential paths must not be symbolic links.")
    for part in path.relative_to(boundary).parts[:-1]:
        current = current / part
        if current.is_symlink():
            raise ValueError("Agent credential paths must not be symbolic links.")
        if current.exists() and not current.is_dir():
            raise ValueError("Invalid agent credential directory.")


def read_private(path, boundary, *, missing=False):
    path = Path(path)
    _check_path(path, boundary)
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        if missing:
            return None
        raise ValueError("No reusable native credentials are available.") from None
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_size > MAX_CREDENTIAL_BYTES:
            raise ValueError("Invalid native credential file.")
        with os.fdopen(fd, "rb", closefd=False) as stream:
            raw = stream.read(MAX_CREDENTIAL_BYTES + 1)
        if len(raw) > MAX_CREDENTIAL_BYTES:
            raise ValueError("Native credential file exceeds the supported limit.")
        value = json.loads(raw)
        if not isinstance(value, dict):
            raise ValueError
        return value
    except (ValueError, UnicodeError):
        raise ValueError("Invalid native credential file.") from None
    finally:
        os.close(fd)


def write_private(path, value, boundary):
    path = Path(path)
    _check_path(path, boundary)
    current = Path(boundary)
    for part in path.relative_to(current).parts[:-1]:
        current = private_directory(current / part)
    if path.is_symlink():
        raise ValueError("Agent credential paths must not be symbolic links.")
    raw = json.dumps(value, separators=(",", ":")).encode()
    if len(raw) > MAX_CREDENTIAL_BYTES:
        raise ValueError("Native credential file exceeds the supported limit.")
    fd, temporary = tempfile.mkstemp(prefix=".agent-", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _token(value):
    return isinstance(value, str) and 8 <= len(value) <= 16384 and not any(ord(c) <= 32 for c in value)


def _expiry(token):
    """Unverified JWT expiry is scheduling metadata, never authentication proof."""
    if not isinstance(token, str) or len(token) > 16384:
        return None
    try:
        part = token.split(".")[1]
        payload = json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
        value = payload.get("exp")
        return value if type(value) in {int, float} and 0 < value < 253402300799 else None
    except (ValueError, IndexError, TypeError):
        return None


def _iso(seconds):
    return datetime.fromtimestamp(seconds, timezone.utc).isoformat() if seconds is not None else None


def host_binding(host):
    value = {"target": host.target, "project": host.project, "pool": host.pool, "guest": asdict(host.guest)}
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def native_material(provider, native):
    """Validate a native cache and select only access credentials for a clone."""
    provider_id(provider)
    if not isinstance(native, dict):
        raise ValueError("No reusable native credentials are available.")
    if provider == "codex":
        tokens = native.get("tokens")
        if (not isinstance(tokens, dict) or native.get("auth_mode", "chatgpt") not in {"chatgpt", "chatgptAuthTokens"}
                or native.get("OPENAI_API_KEY") or not all(_token(tokens.get(k)) for k in ("access_token", "id_token"))
                or not isinstance(tokens.get("account_id"), str) or not 1 <= len(tokens["account_id"]) <= 256
                or any(ord(c) < 32 for c in tokens["account_id"])):
            raise ValueError("A native ChatGPT connection is required for Codex delegation.")
        # Pinned Codex 0.163.0 natively loads this external-auth mode and does
        # not try to refresh it. It is an experimental compatibility format.
        clean = {"auth_mode": "chatgptAuthTokens", "OPENAI_API_KEY": None,
                 "tokens": {k: tokens[k] for k in ("id_token", "access_token", "account_id")},
                 "last_refresh": native.get("last_refresh") or datetime.now(timezone.utc).isoformat()}
        clean["tokens"]["refresh_token"] = ""
        return {"kind": "native", "content": clean, "expires_at": _expiry(tokens["access_token"])}
    if provider == "claude":
        value = native.get("claudeAiOauth")
        if not isinstance(value, dict) or not _token(value.get("accessToken")):
            raise ValueError("A native Claude subscription connection is required for delegation.")
        expiry = value.get("expiresAt")
        if type(expiry) not in {int, float} or not 0 < expiry < 253402300799000:
            raise ValueError("The native Claude credential has no supported expiration metadata.")
        return {"kind": "environment", "content": {"CLAUDE_CODE_OAUTH_TOKEN": value["accessToken"]},
                "expires_at": expiry / 1000}
    if not _token(native.get("accessToken")):
        raise ValueError("A native Cursor account connection is required for delegation.")
    # Pinned Cursor's own --auth-token/CURSOR_AUTH_TOKEN implementation stores
    # setAuthentication(accessToken, accessToken). Both values below are the
    # same access credential; no browser refresh credential is delegated.
    return {"kind": "native", "content": {"accessToken": native["accessToken"], "refreshToken": native["accessToken"]},
            "expires_at": _expiry(native["accessToken"])}


def controller_environment(home, binaries):
    # Never inherit a caller's auth/config overrides or execute code in its cwd.
    paths = list(dict.fromkeys(str(Path(p).parent) for p in binaries.values() if p))
    return {"HOME": str(home), "USER": "enginebox", "LOGNAME": "enginebox",
            "PATH": ":".join(paths + ["/usr/local/bin", "/usr/bin", "/bin"]),
            "XDG_CONFIG_HOME": str(home / ".config"), "XDG_CACHE_HOME": str(home / ".cache"),
            "XDG_DATA_HOME": str(home / ".local/share"), "CODEX_HOME": str(home / ".codex"),
            "CLAUDE_CONFIG_DIR": str(home / ".claude"), "NO_OPEN_BROWSER": "1",
            "BROWSER": "/usr/bin/true", "TERM": "xterm-256color", "LANG": "C.UTF-8"}


class _SetupToken:
    def __init__(self, home):
        self.home, self.value = home, None

    def output(self, visible):
        match = re.search(r"(?<![A-Za-z0-9_-])sk-ant-oat01-[A-Za-z0-9_-]{20,4096}(?=[\s\"']|$)", visible)
        if match:
            self.value = match[0]

    def complete(self, environment, cancelled):
        if self.value is None or cancelled():
            return guest_auth.provider_status("claude")
        status = guest_auth.probe("claude", {**environment, "CLAUDE_CODE_OAUTH_TOKEN": self.value}, cancelled=cancelled)
        if status["status"] == "connected" and not cancelled():
            write_private(self.home / TOKEN_FILE, {"kind": "setup_token", "value": self.value,
                          "expires_at": time.time() + 364 * 86400}, self.home)
            status.update(auth_method="subscription")
        return status


def controller_supervise(home, provider, mode, binaries):
    """Child entry point: all token-bearing output remains inside this process."""
    os.umask(0o077)
    home = private_directory(home)
    # Codex rejects an explicitly configured CODEX_HOME that does not exist,
    # including for login/status. Create only this controller's private cache.
    private_directory(home / ".codex")
    private_directory(home / ".claude")
    os.chdir(home)
    environment = controller_environment(home, binaries)
    os.environ.clear()
    os.environ.update(environment)
    for item, path in binaries.items():
        guest_auth.COMMANDS[item] = {**guest_auth.COMMANDS[item], "binary": path or "/nonexistent/enginebox-native-client"}
    guest_auth.COMMANDS["codex"] = {**guest_auth.COMMANDS["codex"],
        "login": ["-c", 'cli_auth_credentials_store="file"', "login", "--device-auth"],
        "status": ["-c", 'cli_auth_credentials_store="file"', "login", "status"]}
    if mode == "login":
        if provider == "claude":
            token = _SetupToken(home)
            guest_auth.login(provider, login_args=["setup-token"], output_hook=token.output, completion_hook=token.complete)
        else:
            guest_auth.login(provider)
        return
    descriptor = None
    try:
        # Some native status implementations may refresh their own cache. They
        # must share the login/delegation lock, including across controllers.
        descriptor = guest_auth.provider_lock(provider)
        saved = read_private(home / TOKEN_FILE, home, missing=True)
        if saved and _token(saved.get("value")):
            environment[{"claude": "CLAUDE_CODE_OAUTH_TOKEN", "cursor": "CURSOR_API_KEY"}.get(provider, "ENGINEBOX_UNUSED")] = saved["value"]
        if saved and provider == "cursor" and binaries.get(provider):
            code, output = guest_auth.capture([binaries[provider], "status", "--format", "json"], environment)
            status = guest_auth.parse_status(provider, code, output, {})
        else:
            status = guest_auth.probe(provider, environment)
        if saved and status["status"] == "connected":
            status["auth_method"] = "subscription" if provider == "claude" else "api_key"
    except (OSError, ValueError, TimeoutError, subprocess.SubprocessError):
        status = guest_auth.provider_status(provider)
    finally:
        if descriptor is not None:
            os.close(descriptor)
    guest_auth.emit({"type": "statuses", "providers": [status]})


class _ControllerGuard:
    def __init__(self, access):
        self.access = access

    def account_workspace(self, host_id, name):
        if host_id != "controller" or name not in PROVIDERS:
            raise ValueError("Unknown controller connection.")
        return self.access.home(name)


# Fixed guest operations. Credential JSON travels only in stdin/stdout over the
# configured Incus/SSH transport; neither filenames nor commands come from users.
GUEST_TRANSFER = r'''
import json, os, pathlib, stat, sys, tempfile, shlex, subprocess, hashlib, re
LIMIT = 65536
root = pathlib.Path.home()
IDENTITY_OWNER = 0
IDENTITY_PATHS = ("/var/lib/enginebox/instance-uuid", "/etc/machine-id")
paths = {"codex": ".codex/auth.json", "claude": ".claude/.credentials.json", "cursor": ".config/cursor/auth.json"}
def guest_identity():
    values = []
    for filename in IDENTITY_PATHS:
        fd = os.open(filename, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != IDENTITY_OWNER or info.st_mode & 0o022 or info.st_size > 128: raise ValueError()
            value = os.read(fd, 129).decode("ascii").strip().lower()
            if not re.fullmatch(r"[0-9a-f]{32}", value) or value == "0" * 32: raise ValueError()
            values.append(value)
        finally: os.close(fd)
    if values[0] != values[1]: raise ValueError()
    return values[0]
def safe_path(relative, create=False):
    path = root / relative
    current = root
    if current.is_symlink(): raise ValueError()
    for part in pathlib.Path(relative).parts[:-1]:
        current = current / part
        if create: current.mkdir(mode=0o700, exist_ok=True)
        info = current.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid(): raise ValueError()
    if path.is_symlink(): raise ValueError()
    return path
def read(relative, missing=False):
    try: path = safe_path(relative)
    except FileNotFoundError:
        if missing: return {}
        raise
    try: fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        if missing: return {}
        raise
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_size > LIMIT: raise ValueError()
        with os.fdopen(fd, "rb", closefd=False) as stream: raw = stream.read(LIMIT + 1)
        if len(raw) > LIMIT: raise ValueError()
        value = json.loads(raw)
        if not isinstance(value, dict): raise ValueError()
        return value
    finally: os.close(fd)
def write(relative, value, raw=False):
    path = safe_path(relative, True)
    encoded = value.encode() if raw else json.dumps(value, separators=(",", ":")).encode()
    if len(encoded) > LIMIT: raise ValueError()
    fd, temporary = tempfile.mkstemp(prefix=".enginebox-", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(encoded); stream.flush(); os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary): os.unlink(temporary)
def content_hash(content):
    return hashlib.sha256(json.dumps(content, sort_keys=True).encode()).hexdigest()
def current_content(provider, kind):
    if provider == "claude":
        return {"CLAUDE_CODE_OAUTH_TOKEN": read(".claude/settings.json", True).get("env", {}).get("CLAUDE_CODE_OAUTH_TOKEN")}
    if provider == "cursor" and kind == "environment":
        path = safe_path(".config/enginebox/agents/cursor.env")
        words = shlex.split(path.read_text())
        if len(words) != 2 or words[0] != "export" or not words[1].startswith("CURSOR_API_KEY="): raise ValueError()
        return {"CURSOR_API_KEY": words[1].split("=", 1)[1]}
    return read(paths[provider], True)
try:
    os.umask(0o077)
    data = sys.stdin.buffer.read(LIMIT + 1)
    if len(data) > LIMIT: raise ValueError()
    request = json.loads(data)
    provider, operation = request["provider"], request["operation"]
    if provider not in paths: raise ValueError()
    instance_id = guest_identity()
    if operation == "export":
        print(json.dumps({"ok": True, "native": read(paths[provider]), "instance_id": instance_id}, separators=(",", ":")))
    elif operation == "attach":
        content, generation = request["content"], request["generation"]
        if not isinstance(generation, str) or len(generation) != 64: raise ValueError()
        if request.get("expected_instance_id") and request["expected_instance_id"].replace("-", "") != instance_id: raise ValueError()
        marker_path = ".config/enginebox/agents/" + provider + ".json"
        marker = read(marker_path, True)
        if not marker and not request.get("retire_refresh"):
            existing = read(paths[provider], True)
            has_native = bool(existing.get("tokens") or existing.get("OPENAI_API_KEY")) if provider == "codex" else bool(existing.get("claudeAiOauth")) if provider == "claude" else bool(existing.get("accessToken") or existing.get("refreshToken"))
            env = read(".claude/settings.json", True).get("env", {}) if provider == "claude" else {}
            has_override = any(env.get(k) for k in ("CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"))
            if has_native or has_override: raise ValueError()
        if provider == "claude" and marker and not request.get("retire_refresh"):
            if read(paths[provider], True).get("claudeAiOauth"): raise ValueError()
            env = read(".claude/settings.json", True).get("env", {})
            if env.get("ANTHROPIC_API_KEY") or env.get("ANTHROPIC_AUTH_TOKEN"): raise ValueError()
        if not request.get("retire_refresh") and marker.get("content_hash") and marker["content_hash"] != content_hash(current_content(provider, marker.get("kind"))):
            # A workspace-native login or manual change wins over background
            # reconciliation. Explicitly adopt/reconnect it before overwriting.
            raise ValueError()
        if marker.get("generation") == generation:
            print(json.dumps({"ok": True, "unchanged": True, "instance_id": instance_id}))
            sys.exit(0)
        expected = request.get("expected_version")
        if expected:
            binary = "codex" if provider == "codex" else "agent"
            version = subprocess.run([binary, "--version"], stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
            if version.returncode or expected not in version.stdout.decode(errors="replace").split(): raise ValueError()
        if provider == "claude":
            if set(content) != {"CLAUDE_CODE_OAUTH_TOKEN"}: raise ValueError()
            settings = read(".claude/settings.json", True)
            env = settings.get("env", {})
            if not isinstance(env, dict): raise ValueError()
            settings["env"] = {**env, **content}
            write(".claude/settings.json", settings)
            if request.get("retire_refresh"):
                # Adoption moves refresh ownership to the controller. The
                # source now uses the same supported access-only environment.
                cache = safe_path(paths[provider])
                if cache.exists():
                    read(paths[provider])
                    cache.unlink()
        elif provider == "cursor" and request.get("kind") == "environment":
            if set(content) != {"CURSOR_API_KEY"}: raise ValueError()
            relative = ".config/enginebox/agents/cursor.env"
            write(relative, "export CURSOR_API_KEY=" + shlex.quote(content["CURSOR_API_KEY"]) + "\n", True)
            line = "\n# Engine Box native Cursor credentials\n. " + shlex.quote(str(root / relative)) + "\n"
            for filename in (".profile", ".bashrc"):
                path = safe_path(filename)
                prior = path.read_text() if path.exists() else ""
                if len(prior) > LIMIT: raise ValueError()
                if line not in prior: write(filename, prior + line, True)
        else:
            if provider == "codex" and content.get("tokens", {}).get("refresh_token"): raise ValueError()
            if provider == "cursor" and content.get("refreshToken") != content.get("accessToken"): raise ValueError()
            write(paths[provider], content)
        write(marker_path, {"generation": generation, "expires_at": request.get("expires_at"), "refresh_credentials": False,
                            "kind": request.get("kind"), "content_hash": content_hash(content)})
        print(json.dumps({"ok": True, "instance_id": instance_id}))
    else: raise ValueError()
except Exception:
    print('{"ok":false}')
    sys.exit(1)
'''


def transfer(host, name, request):
    box, guest = host.box(), host.box().guest
    command = box.command("exec", name, "--mode=non-interactive", f"--user={guest.uid}", f"--group={guest.gid}",
                          f"--cwd={guest.home}", f"--env=HOME={guest.home}",
                          f"--env=PATH={guest.home}/.local/bin:/usr/local/bin:/usr/bin:/bin", "--", "python3", "-I", "-c", GUEST_TRANSFER)
    raw = json.dumps(request, separators=(",", ":")).encode()
    if len(raw) > MAX_CREDENTIAL_BYTES:
        raise ValueError("Native credential transfer exceeds the supported limit.")
    try:
        result = subprocess.run(command, input=raw, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=20)
        if result.returncode or len(result.stdout) > MAX_CREDENTIAL_BYTES:
            raise ValueError
        value = json.loads(result.stdout)
        if not isinstance(value, dict) or value.get("ok") is not True:
            raise ValueError
        return value
    except (OSError, ValueError, subprocess.SubprocessError):
        raise ValueError("The private native credential transfer failed.") from None


class AgentAccess:
    def __init__(self, fleet, state_dir, *, binaries=None):
        self.fleet = fleet
        self.root = private_directory(Path(state_dir).expanduser().absolute())
        self.homes = private_directory(self.root / "homes")
        self.receipts = private_directory(self.root / "attachments")
        self.binaries = {}
        for provider in PROVIDERS:
            provided = (binaries or {}).get(provider)
            path = provided or shutil.which(guest_auth.COMMANDS[provider]["binary"], path=str(self.root / "bin") + ":/usr/local/bin:/usr/bin:/bin")
            if path and (not os.path.isabs(path) or any(ord(c) < 32 for c in path)):
                raise ValueError("Native client binaries must use absolute paths.")
            self.binaries[provider] = path
        self.manager = AccountManager(_ControllerGuard(self), command_factory=self._command)
        self._lock = threading.RLock()
        self._usage = UsageCache()

    def home(self, provider):
        return private_directory(self.homes / provider_id(provider))

    def _command(self, home, provider, mode, _requested):
        source = ("import sys;sys.path.insert(0," + repr(str(Path(__file__).parent)) + ");"
                  "from agent_access import controller_supervise;"
                  "controller_supervise(" + repr(str(home)) + "," + repr(provider) + "," + repr(mode) + "," + repr(self.binaries) + ")")
        return [sys.executable, "-I", "-u", "-c", source]

    @contextmanager
    def _provider_lock(self, provider):
        if self.manager.active("controller", provider):
            raise ValueError("This provider connection is busy; retry shortly.")
        home = self.home(provider)
        directory = private_directory(home / ".local")
        directory = private_directory(directory / "state")
        directory = private_directory(directory / "enginebox")
        fd = os.open(directory / ("auth-" + provider + ".lock"), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        try:
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
                raise ValueError("Unsafe controller credential lock.")
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ValueError("This provider connection is busy; retry shortly.") from None
            yield home
        finally:
            os.close(fd)

    @staticmethod
    def _public_session(value):
        result = {k: v for k, v in value.items() if k not in {"host_id", "name"}}
        result["scope"] = "controller"
        result["message"] = result["message"].replace("in this workspace", "on the controller").replace("this workspace", "Engine Box").replace("in the workspace", "on the controller")
        return result

    def start(self, provider):
        provider_id(provider)
        if (self.home(provider) / "pending-adoption.json").exists():
            raise ValueError("Finish the pending controller adoption before reconnecting this provider.")
        return self._public_session(self.manager.start("controller", provider, provider))

    def session(self, provider, session_id):
        return self._public_session(self.manager.session("controller", provider, provider, session_id))

    def input(self, provider, session_id, text):
        return self._public_session(self.manager.input("controller", provider, provider, session_id, text))

    def cancel(self, provider, session_id):
        return self._public_session(self.manager.cancel("controller", provider, provider, session_id))

    def _material(self, provider):
        home = self.home(provider)
        if (home / "pending-adoption.json").exists():
            raise ValueError("Finish the pending controller adoption before delegating this provider.")
        saved = read_private(home / TOKEN_FILE, home, missing=True)
        if saved and _token(saved.get("value")) and provider in {"claude", "cursor"}:
            key = "CLAUDE_CODE_OAUTH_TOKEN" if provider == "claude" else "CURSOR_API_KEY"
            return {"kind": "environment", "content": {key: saved["value"]}, "expires_at": saved.get("expires_at")}
        return native_material(provider, read_private(home / NATIVE_FILES[provider], home, missing=True))

    def _credential_digest(self, provider):
        home = self.home(provider)
        saved = read_private(home / TOKEN_FILE, home, missing=True)
        value = saved or read_private(home / NATIVE_FILES[provider], home, missing=True)
        return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()

    def _refresh_blocked(self, provider):
        home = self.home(provider)
        failure = read_private(home / "refresh-failure.json", home, missing=True)
        return bool(failure and failure.get("credential_digest") == self._credential_digest(provider))

    def _record_refresh_failure(self, provider):
        home = self.home(provider)
        write_private(home / "refresh-failure.json", {"credential_digest": self._credential_digest(provider), "failed_at": time.time()}, home)

    def _status(self, provider):
        supplied = self.manager.status("controller", provider)["providers"]
        result = next(p for p in supplied if p["id"] == provider)
        result["scope"] = "controller"
        if "session" in result:
            result["session"] = self._public_session(result["session"])
        result["delegation_status"] = "unknown"
        try:
            material = self._material(provider)
            expiry = material["expires_at"]
            if expiry is not None:
                result["expires_at"] = _iso(expiry)
            result["delegation_status"] = "reauth_required" if self._refresh_blocked(provider) or expiry is not None and expiry <= time.time() else "ready"
            if provider in {"codex", "cursor"} and material["kind"] == "native":
                result["detail"] = "Access-only delegation uses the tested native client format; refresh credentials remain on the controller."
        except ValueError:
            result["delegation_status"] = "reauth_required" if result["status"] == "disconnected" else "unsupported"
        return result

    def status(self):
        with ThreadPoolExecutor(max_workers=3) as executor:
            providers = list(executor.map(self._status, PROVIDERS))
        return {"providers": providers, "scope": "controller", "credential_storage": "private_files"}

    def usage(self, provider):
        provider_id(provider)
        if provider != "codex":
            return usage_unavailable(provider)
        try:
            with self._provider_lock(provider) as home:
                native = read_private(home / NATIVE_FILES[provider], home, missing=True)
                if not native or native.get("OPENAI_API_KEY") or not native.get("tokens"):
                    return usage_unavailable(provider)
                executable = self.binaries.get(provider)
                if not executable:
                    return usage_unavailable(provider, "Install the native Codex client to read account allowance.")
                environment = controller_environment(home, self.binaries)
                return self._usage.read(provider, self._credential_digest(provider),
                    lambda: read_codex(executable, environment))
        except (OSError, ValueError):
            return usage_unavailable(provider, "Usage is temporarily unavailable. Check the connection or retry shortly.")

    def adopt(self, host_id, name, provider):
        provider_id(provider)
        host = self.fleet.account_workspace(host_id, name)
        with self._provider_lock(provider) as home:
            pending_path = home / "pending-adoption.json"
            pending = read_private(pending_path, home, missing=True)
            binding = host_binding(host)
            if pending:
                if pending.get("host_binding") != binding or pending.get("name") != name:
                    raise ValueError("Finish the pending adoption from its original workspace first.")
                native, instance_id = pending["native"], pending["instance_id"]
            else:
                exported = transfer(host, name, {"operation": "export", "provider": provider})
                native, instance_id = exported.get("native"), exported.get("instance_id")
                native_material(provider, native)
                if not isinstance(instance_id, str) or not re.fullmatch(r"[0-9a-f-]{32,36}", instance_id):
                    raise ValueError("The source workspace identity could not be verified.")
                write_private(pending_path, {"host_binding": binding, "name": name, "instance_id": instance_id, "native": native}, home)
            material = native_material(provider, native)
            request = self._attachment(provider, material, retire_refresh=True)
            request["expected_instance_id"] = instance_id
            transfer(host, name, request)
            write_private(home / NATIVE_FILES[provider], native, home)
            saved = home / TOKEN_FILE
            if saved.exists():
                read_private(saved, home)
                saved.unlink()
            pending_path.unlink()
            self._register_attachment(host_id, name, provider, instance_id, binding)
            self.manager._cache.clear()
        return {"provider": provider, "scope": "controller", "status": "adopted"}

    def connect_key(self, provider, text):
        if provider != "cursor" or not _token(text):
            raise ValueError("Only a bounded native Cursor API key is accepted here.")
        with self._provider_lock(provider) as home:
            write_private(home / TOKEN_FILE, {"kind": "personal_api_key", "value": text, "expires_at": None}, home)
            self.manager._cache.clear()
        return {"provider": provider, "scope": "controller", "status": "saved"}

    def _refresh(self, provider, home):
        native = read_private(home / NATIVE_FILES[provider], home)
        environment = controller_environment(home, self.binaries)
        executable = self.binaries.get(provider)
        if not executable:
            raise ValueError("Install the native client on the controller to refresh this connection.")
        if provider == "codex":
            _refresh_codex(executable, environment)
        elif provider == "claude":
            oauth = native.get("claudeAiOauth", {})
            if (not _token(oauth.get("refreshToken")) or not isinstance(oauth.get("scopes"), list)
                    or not oauth["scopes"] or any(not isinstance(scope, str) or not re.fullmatch(r"[a-zA-Z0-9_:.-]{1,128}", scope) for scope in oauth["scopes"])):
                raise ValueError("Reconnect this provider to renew its delegated access.")
            environment.update(CLAUDE_CODE_OAUTH_REFRESH_TOKEN=oauth["refreshToken"], CLAUDE_CODE_OAUTH_SCOPES=" ".join(oauth["scopes"]))
            code, _ = guest_auth.capture([executable, "auth", "login", "--claudeai"], environment, timeout=20)
            if code:
                raise ValueError("Reconnect this provider to renew its delegated access.")
        else:
            # The native client owns its undocumented refresh implementation.
            # Never reproduce a private refresh endpoint in this controller.
            guest_auth.capture([executable, "status", "--format", "json"], environment, timeout=15)

    def attach(self, host_id, name, providers=None, *, expected_instance_id=None, expected_host_binding=None):
        host = self.fleet.account_workspace(host_id, name)
        binding = host_binding(host)
        key = hashlib.sha256((host_id + ":" + name).encode()).hexdigest()
        prior = read_private(self.receipts / (key + ".json"), self.root, missing=True)
        if prior:
            expected_host_binding = expected_host_binding or prior.get("host_binding")
            expected_instance_id = expected_instance_id or prior.get("instance_id")
        if expected_host_binding is not None and expected_host_binding != binding:
            raise ValueError("The registered workspace host has changed; attach this workspace explicitly.")
        if providers is not None and (not isinstance(providers, (list, tuple)) or not providers
                or any(not isinstance(p, str) for p in providers) or len(set(providers)) != len(providers)):
            raise ValueError("Select distinct native agent providers.")
        selected = PROVIDERS if providers is None else tuple(provider_id(p) for p in providers)
        results = []
        for provider in selected:
            result = {"provider": provider, "status": "unavailable"}
            try:
                with self._provider_lock(provider) as home:
                    material = self._material(provider)
                    expiry = material["expires_at"]
                    if expiry is not None and expiry <= time.time() + 120:
                        result["status"] = "reauth_required"
                        if self._refresh_blocked(provider):
                            raise ValueError("Reconnect this provider after its failed native refresh.")
                        try:
                            self._refresh(provider, home)
                        except (OSError, ValueError, TimeoutError, subprocess.SubprocessError):
                            self._record_refresh_failure(provider)
                            raise
                        material = self._material(provider)
                        expiry = material["expires_at"]
                    if expiry is not None and expiry <= time.time() + 30:
                        self._record_refresh_failure(provider)
                        result.update(status="reauth_required", detail="Reconnect this provider before delegating a new session.")
                    else:
                        request = self._attachment(provider, material)
                        request["expected_instance_id"] = expected_instance_id
                        generation = request["generation"]
                        delivered = transfer(host, name, request)
                        result.update(status="unchanged" if delivered.get("unchanged") else "attached", generation=generation, refresh_credentials=False)
                        if expiry is not None:
                            result["expires_at"] = _iso(expiry)
                        instance_id = delivered.get("instance_id")
                        if isinstance(instance_id, str) and re.fullmatch(r"[0-9a-f-]{32,36}", instance_id):
                            self._register_attachment(host_id, name, provider, instance_id, binding)
            except (OSError, ValueError, TimeoutError, subprocess.SubprocessError):
                result.update(detail="This provider could not be delegated; check its controller connection.")
            results.append(result)
        return {"host_id": host_id, "name": name, "providers": results}

    def _register_attachment(self, host_id, name, provider, instance_id, binding):
        key = hashlib.sha256((host_id + ":" + name).encode()).hexdigest()
        with self._lock:
            path = self.receipts / (key + ".json")
            current = read_private(path, self.root, missing=True) or {}
            providers = current.get("providers", []) if current.get("instance_id") == instance_id and current.get("host_binding") == binding else []
            value = {"host_id": host_id, "name": name, "instance_id": instance_id, "host_binding": binding,
                     "providers": sorted(set(providers) | {provider})}
            write_private(path, value, self.root)

    def reconcile(self, attach_callback=None):
        """Refresh registered workspaces; callback should hold fleet host locks.

        New native processes read the updated cache/settings. This does not hot
        replace an access token already cached by a running native process.
        """
        attach_callback = attach_callback or self.attach
        results = []
        for path in sorted(self.receipts.glob("*.json"))[:256]:
            try:
                value = read_private(path, self.root)
                result = attach_callback(value["host_id"], value["name"], value["providers"],
                                         expected_instance_id=value["instance_id"], expected_host_binding=value["host_binding"])
                results.append(result)
            except (OSError, ValueError, KeyError, TypeError):
                continue  # stopped/unassigned/deleted/rebound workspaces remain untouched
        return {"workspaces": results}

    @staticmethod
    def _attachment(provider, material, *, retire_refresh=False):
        generation = hashlib.sha256(json.dumps(material, sort_keys=True).encode()).hexdigest()
        expected = {"codex": CODEX_VERSION, "cursor": CURSOR_VERSION}.get(provider) if material["kind"] == "native" else None
        return {"operation": "attach", "provider": provider, "generation": generation,
                "expected_version": expected, "retire_refresh": retire_refresh, **material}

    def close(self):
        self.manager.close()


def _refresh_codex(executable, environment):
    """Ask the official native app-server to refresh its own central auth cache."""
    process = subprocess.Popen([executable, "-c", 'cli_auth_credentials_store="file"', "app-server"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL, env=environment, start_new_session=True)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    buffer, deadline = b"", time.monotonic() + 20
    def send(value):
        process.stdin.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
        process.stdin.flush()
    try:
        send({"id": 1, "method": "initialize", "params": {"clientInfo": {"name": "enginebox", "version": "0.1.0"}}})
        while time.monotonic() < deadline:
            for key, _ in selector.select(.2):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    raise ValueError
                buffer += chunk
                if len(buffer) > MAX_CREDENTIAL_BYTES:
                    raise ValueError
                while b"\n" in buffer:
                    line, buffer = buffer.split(b"\n", 1)
                    value = json.loads(line)
                    if value.get("id") == 1:
                        if "error" in value:
                            raise ValueError
                        send({"method": "initialized"})
                        send({"id": 2, "method": "account/read", "params": {"refreshToken": True}})
                    elif value.get("id") == 2:
                        if "error" in value:
                            raise ValueError
                        return
        raise TimeoutError
    except (OSError, ValueError, TimeoutError):
        raise ValueError("Reconnect Codex to renew its delegated access.") from None
    finally:
        selector.close()
        process.stdin.close()
        guest_auth.stop_group(process)
        process.stdout.close()
