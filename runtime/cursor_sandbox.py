"""Application-scoped Cursor Agent sandbox policy and model-free guest preflight."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import selectors
import signal
import stat
import subprocess
import sys
import time


PROFILE_NAME = "enginebox_cursor_sandbox"
PROFILE_FILE = "enginebox-cursor-sandbox"


def profile_text(home):
    # AppArmor treats glob characters inside quoted attachment paths specially.
    # Fail rather than turning a configured home into a broad policy expression.
    if (not isinstance(home, str) or not re.fullmatch(r"/[A-Za-z0-9_./-]+", home)
            or ".." in Path(home).parts or not home.rstrip("/") or str(Path(home)) != home):
        raise ValueError("Cursor sandbox policy needs an absolute home using letters, digits, /, _, . or -.")
    helper = home.rstrip("/") + "/.local/share/cursor-agent/versions/*/cursorsandbox"
    return f'''# Engine Box: Cursor Agent CLI sandbox helper only.
profile {PROFILE_NAME} "{helper}" flags=(attach_disconnected) {{
  file,
  /** ix,
  capability sys_admin,
  capability net_admin,
  capability chown,
  capability setuid,
  capability setgid,
  capability setpcap,
  userns,
  network unix,
  network inet stream,
  network inet6 stream,
  network inet dgram,
  network inet6 dgram,
  network netlink raw,
  signal (receive) peer=unconfined,
  signal (send, receive) peer={PROFILE_NAME},
  mount,
  remount,
  umount,
}}
'''


def prepare_profile(home, directory=Path("/etc/apparmor.d"), *, apply=False):
    text = profile_text(home)
    directory = Path(directory)
    if directory.is_symlink() or not directory.is_dir():
        raise ValueError("AppArmor profile directory must be a regular directory.")
    path = directory / PROFILE_FILE
    if path.is_symlink() or path.exists() and not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError("Refusing a non-regular Cursor AppArmor profile.")
    for mode in ("disable", "force-complain"):
        override = directory / mode / PROFILE_FILE
        if override.exists() or override.is_symlink():
            raise ValueError("Cursor profile has a local mode override; reconcile it before applying.")
    created = not path.exists()
    if not created:
        info = path.stat()
        if info.st_uid != os.geteuid() or info.st_mode & 0o022 or info.st_size > 65536:
            raise ValueError("Existing Cursor profile must be owned by the applying user and not writable by others.")
        attachment = text.splitlines()[1]
        if attachment not in path.read_text().splitlines():
            raise ValueError("Existing Cursor profile has an unexpected attachment; reconcile it explicitly.")
    if created and apply:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
        with os.fdopen(fd, "w") as stream:
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
    # Existing regular profiles are operator policy. Preserve them and let the
    # native preflight detect incompatible local changes instead of overwriting.
    return {"profile": str(path), "created": created, "applied": apply}


def check_loaded_profiles(loaded, canonical):
    names = {line.split(" ", 1)[0] for line in loaded.splitlines()}
    if "cursor_sandbox_agent_cli" in names:
        raise ValueError("An operator Cursor CLI AppArmor profile already exists; reconcile attachment policy explicitly.")
    if PROFILE_NAME in names and not Path(canonical).exists():
        raise ValueError("The Engine Box profile name is already loaded from another source; reconcile it explicitly.")


def check_cursor_version(command):
    """Check identity without allowing an unrelated `agent` to receive subcommands."""
    process = subprocess.Popen(command + ["--version"], stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    output, deadline = bytearray(), time.monotonic() + 10
    try:
        while selector.get_map():
            if time.monotonic() >= deadline:
                raise ValueError("Cursor version check timed out.")
            for key, _ in selector.select(.1):
                chunk = os.read(key.fd, 1025 - len(output))
                if not chunk:
                    selector.unregister(key.fileobj)
                else:
                    output.extend(chunk)
                    if len(output) > 1024:
                        raise ValueError("Unexpected native version response.")
        process.wait(timeout=max(.1, deadline - time.monotonic()))
        version = output.decode(errors="replace").strip()
        if process.returncode or not re.fullmatch(r"(?:Cursor Agent )?20[0-9]{2}\.[0-9]{2}\.[0-9]{2}-[a-zA-Z0-9]+", version):
            raise ValueError("The installed agent executable does not identify as Cursor Agent.")
        return version
    finally:
        stop_process_group(process)
        selector.close()
        process.stdout.close()


def stop_process_group(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        if process.poll() is None:
            raise
    process.wait(timeout=2)


def check_preflight(command, timeout=30):
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, start_new_session=True)
    try:
        if process.wait(timeout=timeout):
            raise ValueError("Cursor's native sandbox preflight failed; inspect its diagnostic above.")
    finally:
        # Kill the runuser/native helper group on timeout as well as any leaked
        # same-group descendants after the ordinary native command exits.
        stop_process_group(process)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--apply", action="store_true")
    args = parser.parse_args(argv)
    try:
        if sys.platform != "linux":
            raise ValueError("The Cursor guest sandbox repair requires Linux; it is qualified on Ubuntu 24.04.")
        import pwd  # Guest repair only; importing this package is portable.
        if not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", args.user):
            raise ValueError("Provide the configured non-root guest user.")
        user = pwd.getpwnam(args.user)
        if user.pw_uid == 0 or user.pw_dir != args.home:
            raise ValueError("The native home must belong to the configured non-root guest user.")
        workspace = Path(args.workspace)
        if (not workspace.is_absolute() or not workspace.is_dir()
                or any(ord(char) < 32 or ord(char) == 127 for char in args.workspace)):
            raise ValueError("Provide the existing absolute guest workspace.")
        profile_text(args.home)
        command = ["runuser", "-u", args.user, "--", "env", "-i", f"HOME={args.home}",
                   f"USER={args.user}", f"LOGNAME={args.user}",
                   f"PATH={args.home}/.local/bin:/usr/local/bin:/usr/bin:/bin", "DISABLE_AUTOUPDATER=1",
                   "/bin/bash", "-c", 'cd "$1" && shift && exec "$@"', "--", args.workspace,
                   args.home + "/.local/bin/agent"]
        # Identity must pass before policy creation/loading, including on reruns.
        version = check_cursor_version(command)
        active = Path("/sys/kernel/security/apparmor").is_dir()
        result = {"apparmor_active": active, "applied": args.apply, "preflight": "not_run", "cursor_version": version}
        if active:
            loaded = Path("/sys/kernel/security/apparmor/profiles").read_text()
            check_loaded_profiles(loaded, Path("/etc/apparmor.d") / PROFILE_FILE)
            if args.apply and os.geteuid() != 0:
                raise ValueError("Applying the guest policy requires root.")
            result.update(prepare_profile(args.home, apply=args.apply))
            if args.apply:
                subprocess.run(["apparmor_parser", "-r", result["profile"]], check=True, timeout=30)
                loaded = Path("/sys/kernel/security/apparmor/profiles").read_text()
                if PROFILE_NAME + " (enforce)" not in loaded.splitlines():
                    raise ValueError("The Cursor helper profile did not enter enforce mode.")
        if args.apply:
            check_preflight(command + ["sandbox", "run", "/bin/true"])
            result["preflight"] = "passed"
        print(json.dumps(result))
        return 0
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        # This command contains only installation paths, never auth or prompts.
        parser.exit(1, f"Cursor sandbox: {error}\n")


if __name__ == "__main__":
    main()
