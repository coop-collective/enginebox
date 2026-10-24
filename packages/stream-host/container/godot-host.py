"""Translate a scoped invitation into local host configuration. Never persist it.

The operator can mount /etc/enginebox/engine.json to select an installed engine
and /project for project files. Neither the browser nor the invitation chooses
the executable, arguments, working directory, or environment.
"""
import json
import os
from pathlib import Path
import sys
import tempfile

STATE = Path("/var/lib/enginebox-stream")


def main():
    if sys.argv[1:] not in ([], ["enroll"]):
        raise ValueError("Usage: enginebox-godot-host [enroll]")
    STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    identity_path = STATE / "identity.json"
    identity = None
    if identity_path.exists():
        if identity_path.is_symlink() or identity_path.stat().st_mode & 0o077:
            raise ValueError("Identity must be private and not a symlink")
        identity = json.loads(identity_path.read_text())
    invitation = b""
    if not identity or not identity.get("enrolled"):
        print("Paste or pipe the downloaded enrollment JSON, then close stdin. It is not saved.", file=sys.stderr)
        invitation = sys.stdin.buffer.read(4097)
        if len(invitation) > 4096:
            raise ValueError("Invitation exceeds size bound")
        scope = json.loads(invitation)
        if not isinstance(scope, dict) or not all(isinstance(scope.get(k), str) for k in ("controller", "project", "invitation")):
            raise ValueError("Expected downloaded enrollment JSON")
    else:
        scope = identity
    config = {
        "controller": scope["controller"], "project": scope["project"],
        "width": 1280, "height": 720, "maxPeers": 4,
        "command": {"executable": "/usr/local/bin/godot", "directory": "/opt/enginebox/godot-check",
                    "args": ["--path", "/opt/enginebox/godot-check", "--rendering-method", "gl_compatibility",
                             "--audio-driver", "Dummy", "--resolution", "1280x720", "--position", "0,0", "--borderless"]},
    }
    operator_file = Path("/etc/enginebox/engine.json")
    if operator_file.exists():
        operator = json.loads(operator_file.read_text())
        if not isinstance(operator, dict) or set(operator) - {"command", "width", "height", "maxPeers", "allowLocalController"}:
            raise ValueError("Unsupported operator configuration keys")
        config.update(operator)
    # Only non-secret configuration enters tmpfs. Native enrollment persists its
    # own random identity atomically before redeeming the short-lived invitation.
    descriptor, path = tempfile.mkstemp(prefix="enginebox-host-", suffix=".json")
    with os.fdopen(descriptor, "w") as file:
        json.dump(config, file)
    read_fd, write_fd = os.pipe()
    os.write(write_fd, invitation)
    os.close(write_fd)
    os.dup2(read_fd, 0)
    os.close(read_fd)
    mode = ["--enroll-only"] if sys.argv[1:] == ["enroll"] else []
    os.execv("/usr/local/bin/enginebox-stream-host", ["enginebox-stream-host", *mode, path, str(STATE)])


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError):
        # Do not echo malformed enrollment documents or private state.
        print("Host configuration failed. Check the enrollment JSON and private state volume.", file=sys.stderr)
        sys.exit(1)
