"""Bounded native CLI invocation inside one private workspace; no agent loop."""
from __future__ import annotations

import json
import os
from pathlib import Path, PurePosixPath
import re
import selectors
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time


PROVIDERS = {"codex": "codex", "claude": "claude", "cursor": "agent"}
SESSION_ID = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
MAX_OUTPUT = 262144
MAX_REQUEST = 16384


def validate_request(value):
    if (not isinstance(value, dict) or not {"provider", "prompt"} <= set(value)
            or set(value) - {"provider", "prompt", "resume_id", "workdir", "timeout_seconds", "model"}):
        raise ValueError("Provide provider, prompt and optional resume_id, workdir, timeout_seconds or model.")
    if not isinstance(value["provider"], str) or value["provider"] not in PROVIDERS:
        raise ValueError("Choose codex, claude or cursor.")
    prompt = value["prompt"]
    if not isinstance(prompt, str) or not prompt.strip() or len(prompt.encode()) > 12000 or "\0" in prompt:
        raise ValueError("Prompt must contain 1–12000 UTF-8 bytes without NUL.")
    result = {"workdir": ".", "timeout_seconds": 120, **value}
    timeout = result["timeout_seconds"]
    if type(timeout) is not int or not 1 <= timeout <= 300:
        raise ValueError("timeout_seconds must be 1–300.")
    workdir = result["workdir"]
    if (not isinstance(workdir, str) or not workdir or len(workdir) > 512
            or PurePosixPath(workdir).is_absolute() or ".." in PurePosixPath(workdir).parts
            or any(ord(char) < 32 or ord(char) == 127 for char in workdir)):
        raise ValueError("workdir must be a relative directory inside the workspace.")
    for key in ("resume_id", "model"):
        item = result.get(key)
        if item is not None and (not isinstance(item, str) or not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_.:-]{0,127}", item)):
            raise ValueError(f"{key} must be a literal native identifier, not a path or option.")
    if result.get("resume_id") is not None and not re.fullmatch(SESSION_ID, result["resume_id"]):
        raise ValueError("Resume requires the exact native session ID.")
    if len(json.dumps(result, ensure_ascii=False).encode()) > MAX_REQUEST:
        raise ValueError("Native launch request is too large after JSON encoding.")
    return result


def native_command(request, executable):
    provider, resume = request["provider"], request.get("resume_id")
    if provider == "codex":
        command = [executable, "exec"] + (["resume", resume] if resume else [])
        command += ["--json", "--skip-git-repo-check", "-c", 'approval_policy="never"',
                    "-c", 'sandbox_mode="workspace-write"']
    elif provider == "claude":
        command = [executable, "-p", "--output-format", "stream-json", "--verbose",
                   "--permission-mode", "acceptEdits", "--permission-prompts", "none", "--max-turns", "20"]
        if resume:
            command += ["--resume", resume]
    else:
        command = [executable, "-p", "--output-format", "stream-json", "--trust", "--sandbox", "enabled"]
        if resume:
            command += ["--resume", resume]
    if request.get("model"):
        command += ["--model", request["model"]]
    # Codex and Claude accept stdin. Cursor documents a positional initial prompt.
    command += ["--", request["prompt"]] if provider == "cursor" else (["-"] if provider == "codex" else [])
    return command


def private_directory(path, boundary):
    current = boundary
    for part in path.relative_to(boundary).parts:
        current /= part
        current.mkdir(mode=0o700, exist_ok=True)
        info = current.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid():
            raise ValueError("Native run state must be an owned directory without symlinks.")
    info = path.lstat()
    if info.st_mode & 0o077:
        raise ValueError("Native run state directory must have mode 0700.")
    return path


def native_environment(home):
    # Never inherit controller credentials, API-key billing overrides, alternate
    # provider homes, proxy settings or an operator MCP bearer token.
    environment = {"HOME": str(home), "USER": os.environ.get("USER", "agent"),
                   "LOGNAME": os.environ.get("USER", "agent"), "SHELL": "/bin/bash",
                   "PATH": f"{home}/.local/bin:/usr/local/bin:/usr/bin:/bin",
                   "LANG": "C.UTF-8", "TERM": "dumb", "NO_OPEN_BROWSER": "1",
                   "BROWSER": "/usr/bin/true", "DISABLE_AUTOUPDATER": "1",
                   "DISPLAY": os.environ.get("DISPLAY", ":99")}
    # Cursor's documented key mode is installed separately from its native
    # browser-token cache. Read that one generated assignment, never source a shell.
    relative = Path(".config/enginebox/agents/cursor.env")
    current = home
    for part in relative.parts:
        current /= part
        if current.is_symlink():
            raise ValueError("Delegated credential paths must not be symlinks.")
    if current.exists():
        info = current.stat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077 or info.st_size > 65536:
            raise ValueError("Invalid delegated credential file.")
        words = shlex.split(current.read_text())
        if len(words) != 2 or words[0] != "export" or not words[1].startswith("CURSOR_API_KEY="):
            raise ValueError("Invalid delegated credential file.")
        environment["CURSOR_API_KEY"] = words[1].split("=", 1)[1]
    return environment


def terminate(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    except PermissionError:
        if process.poll() is None:
            raise
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        pass
    # A descendant can ignore SIGTERM after its leader has exited. Always kill
    # the remaining group before releasing the provider's writer lease.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        if process.poll() is None:
            raise
    process.wait(timeout=2)


def verify_client(executable, provider, environment, cwd):
    """The executable name 'agent' is also used by unrelated products."""
    process = subprocess.Popen([executable, "--version"], cwd=cwd, env=environment,
                               stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, start_new_session=True)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    output, deadline = bytearray(), time.monotonic() + 10
    try:
        while selector.get_map():
            if time.monotonic() >= deadline:
                raise ValueError("Native version preflight timed out.")
            for key, _ in selector.select(.1):
                chunk = os.read(key.fd, 1025 - len(output))
                if not chunk:
                    selector.unregister(key.fileobj)
                else:
                    output.extend(chunk)
                    if len(output) > 1024:
                        raise ValueError("Invalid native version response.")
        process.wait(timeout=max(.1, deadline - time.monotonic()))
        version = output.decode(errors="replace").strip()
        patterns = {"codex": r"codex-cli [0-9]+\.[0-9]+\.[0-9]+(?:[^\n]*)?",
                    "claude": r"[0-9]+\.[0-9]+\.[0-9]+ \(Claude Code\)",
                    "cursor": r"(?:Cursor Agent )?20[0-9]{2}\.[0-9]{2}\.[0-9]{2}-[a-zA-Z0-9]+"}
        if process.returncode or not re.fullmatch(patterns[provider], version):
            raise ValueError("The installed executable does not identify as the expected native client.")
        return version
    finally:
        terminate(process)
        selector.close()
        process.stdout.close()


def execute(request, home, workspace):
    import fcntl  # This module is also imported by Windows operator MCP clients.
    request = validate_request(request)
    home, workspace = Path(home), Path(workspace)
    if not home.is_absolute() or not workspace.is_absolute() or home.is_symlink() or workspace.is_symlink():
        raise ValueError("Native homes and workspace roots must be absolute directories without symlinks.")
    home, workspace = home.resolve(strict=True), workspace.resolve(strict=True)
    cwd = (workspace / request["workdir"]).resolve(strict=True)
    if not cwd.is_relative_to(workspace) or not cwd.is_dir():
        raise ValueError("workdir must remain inside its assigned workspace.")
    state = private_directory(home / ".local/state/enginebox/agent-runs", home)
    lock = os.open(state / (request["provider"] + ".lock"), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(lock)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
            raise ValueError("Unsafe native session lease.")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("This workspace provider already has an active native run.") from None
        environment = native_environment(home)
        executable = shutil.which(PROVIDERS[request["provider"]], path=environment["PATH"])
        if not executable:
            raise ValueError("The native client is not installed in this workspace.")
        verify_client(executable, request["provider"], environment, cwd)
        # A private bounded file avoids blocking on a full stdin pipe when a
        # native client fails before consuming its prompt.
        with tempfile.TemporaryFile() as input_file:
            if request["provider"] != "cursor":
                input_file.write(request["prompt"].encode())
            input_file.seek(0)
            process = subprocess.Popen(native_command(request, executable), cwd=cwd, env=environment,
                                       stdin=input_file, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                       start_new_session=True, pass_fds=(lock,))
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        output, timed_out, truncated = bytearray(), False, False
        deadline = time.monotonic() + request["timeout_seconds"]
        try:
            while selector.get_map():
                if time.monotonic() >= deadline:
                    timed_out = True
                    terminate(process)
                    break
                for key, _ in selector.select(.1):
                    chunk = os.read(key.fd, 8192)
                    if not chunk:
                        selector.unregister(key.fileobj)
                    elif len(output) + len(chunk) > MAX_OUTPUT:
                        output.extend(chunk[:MAX_OUTPUT - len(output)])
                        truncated = True
                        terminate(process)
                        break
                    else:
                        output.extend(chunk)
                if truncated:
                    break
            if process.poll() is None:
                process.wait(timeout=max(.1, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            timed_out = True
        finally:
            terminate(process)
            selector.close()
            process.stdout.close()
        return {"exit_code": process.returncode, "output": output.decode(errors="replace"),
                "timed_out": timed_out, "output_truncated": truncated}
    finally:
        os.close(lock)


def lease_active(home, provider):
    """Read-only guest probe used after uncertain controller/transport failures."""
    import fcntl
    if provider not in PROVIDERS:
        raise ValueError("Unknown native provider.")
    current = Path(home)
    if not current.is_absolute() or current.is_symlink():
        raise ValueError("Invalid private home.")
    for part in (".local", "state", "enginebox", "agent-runs", provider + ".lock"):
        current /= part
        if current.is_symlink():
            raise ValueError("Unsafe native session lease.")
        if not current.exists():
            return False
    fd = os.open(current, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
            raise ValueError("Unsafe native session lease.")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False
    finally:
        os.close(fd)


def main():
    try:
        os.umask(0o077)
        data = sys.stdin.buffer.read(MAX_REQUEST + 1)
        if len(data) > MAX_REQUEST:
            raise ValueError("Native launch request is too large.")
        result = execute(json.loads(data), os.environ["HOME"], sys.argv[1])
        print(json.dumps(result))
    except (OSError, ValueError, KeyError, subprocess.SubprocessError):
        # Provider output is returned only in the authenticated private result;
        # Python exceptions can include sensitive arguments and must not escape.
        print(json.dumps({"error": "Native launch failed; check installation, workspace paths and its active session lease."}))
        sys.exit(1)


if __name__ == "__main__":
    main()
