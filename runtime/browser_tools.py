"""Launch the unmodified Playwright MCP server with a private, locked profile.

This is environment setup, not an agent harness. All browser tools are supplied
by Microsoft's pinned @playwright/mcp package over stdio.
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import sys


def profile_name(value: str) -> str:
    if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,63}", value):
        raise ValueError("Browser profile must contain 1–64 lowercase letters, digits, '-' or '_'.")
    return value


def settings(client: str, profile: str | None = None, headed: bool = False) -> tuple[list[str], Path]:
    client = profile_name(client)
    profile = profile_name(profile or client)
    package = Path(os.environ.get("ENGINEBOX_BROWSER_PACKAGE", "/opt/enginebox-browser/current"))
    root = Path(os.environ.get("ENGINEBOX_BROWSER_HOME", str(Path.home() / ".local/share/enginebox/browser")))
    if not root.is_absolute() or not package.is_absolute():
        raise ValueError("Browser package and data paths must be absolute.")
    root = root.resolve()
    state = root / profile
    if state.is_symlink():
        raise ValueError("Browser profile must not be a symlink.")
    executable_file = package / "chromium-path.txt"
    executable = executable_file.read_text().strip()
    if not executable or not Path(executable).is_absolute():
        raise ValueError("Invalid installed Chromium path.")
    command = ["node", str(package / "node_modules/@playwright/mcp/cli.js"),
               "--executable-path", executable, "--user-data-dir", str(state / "profile"),
               "--output-dir", str(state / "output"), "--viewport-size", "1440x900",
               "--snapshot-mode", "none"]
    # The pinned upstream server writes automatic action snapshots to files.
    # Explicit browser_snapshot calls return inline YAML; disable automatic
    # snapshots to avoid duplicate file output between those useful calls.
    if not headed:
        command.append("--headless")
    # Keep the browser sandbox enabled. A dedicated VM with working unprivileged
    # user namespaces is a prerequisite, not a reason to add --no-sandbox.
    command.append("--sandbox")
    return command, state


def acquire_profile(state: Path) -> int:
    state.mkdir(parents=True, mode=0o700, exist_ok=True)
    os.chmod(state, 0o700)
    for name in ("profile", "output"):
        path = state / name
        if path.is_symlink():
            raise ValueError("Browser state directories must not be symlinks.")
        path.mkdir(mode=0o700, exist_ok=True)
        os.chmod(path, 0o700)
    descriptor = os.open(state / "session.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(descriptor)
        raise RuntimeError("This browser profile is in use. Close its MCP session or choose --profile NAME.") from None
    return descriptor


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", default="manual")
    parser.add_argument("--profile", help="Separate profile for concurrent sessions; defaults to the client name")
    parser.add_argument("--headed", action="store_true", help="Use the guest DISPLAY instead of headless Chromium")
    parser.add_argument("--show-command", action="store_true", help="Print the launch argv without starting a browser")
    args = parser.parse_args(argv)
    command, state = settings(args.client, args.profile, args.headed)
    if args.show_command:
        print(json.dumps(command))
        return 0
    if os.geteuid() == 0:
        raise ValueError("Run browser MCP as the non-root workspace user.")
    descriptor = acquire_profile(state)
    # Keep the profile lock in the actual MCP process, including if its invoking
    # CLI disconnects. No intermediary owns or rewrites the MCP byte stream.
    os.set_inheritable(descriptor, True)
    os.execvpe(command[0], command, os.environ)
    return 0  # exec never returns on success


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, RuntimeError) as error:
        print(f"enginebox-browser: {error}", file=sys.stderr)
        raise SystemExit(1)
