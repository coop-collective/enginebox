"""Guest-side native login supervisor. Emits only a restricted JSON-lines protocol."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import selectors
import shutil
import signal
import stat
import struct
import subprocess
import sys
import termios
import time
from urllib.parse import parse_qsl, urlsplit


PROVIDERS = {"codex": "Codex", "claude": "Claude Code", "cursor": "Cursor Agent"}
COMMANDS = {
    "codex": {"binary": "codex", "login": ["login", "--device-auth"], "status": ["login", "status"]},
    "claude": {"binary": "claude", "login": ["auth", "login", "--claudeai"], "status": ["auth", "status", "--json"]},
    "cursor": {"binary": "agent", "login": ["login"], "status": ["status", "--format", "json"]},
}
OVERRIDES = {
    "codex": {"OPENAI_API_KEY", "CODEX_API_KEY", "OPENAI_ACCESS_TOKEN", "OPENAI_BASE_URL", "AZURE_OPENAI_API_KEY"},
    "claude": {"ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_BASE_URL",
               "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"},
    "cursor": {"CURSOR_API_KEY", "CURSOR_AUTH_TOKEN", "CURSOR_API_URL"},
}
URL_PATHS = {
    "codex": {"auth.openai.com": {"/codex/device"}},
    "claude": {"claude.com": {"/cai/oauth/authorize"}},
    "cursor": {"cursor.com": {"/loginDeepControl"}},
}
MAX_OUTPUT = 65536
MAX_CODE = 2048
MAX_SECONDS = 600


def subscription_environment(provider, environment=None):
    result = dict(os.environ if environment is None else environment)
    for key in set().union(*OVERRIDES.values()):
        result.pop(key, None)
    result.update(NO_OPEN_BROWSER="1", BROWSER="/usr/bin/true", TERM="xterm-256color")
    return result


def authorization_url(provider, value):
    """Only official login endpoints, never token-bearing response/callback URLs."""
    if not isinstance(value, str) or len(value) > 8192 or any(ord(c) <= 32 for c in value):
        return None
    try:
        parsed = urlsplit(value)
        if (parsed.scheme != "https" or parsed.username is not None or parsed.password is not None
                or parsed.port not in {None, 443} or parsed.fragment
                or parsed.path not in URL_PATHS.get(provider, {}).get(parsed.hostname, set())):
            return None
        pairs = parse_qsl(parsed.query, keep_blank_values=True)
        query = dict(pairs)
        allowed = {"codex": set(),
                   "claude": {"client_id", "code", "code_challenge", "code_challenge_method", "redirect_uri", "response_type", "scope", "state"},
                   "cursor": {"challenge", "mode", "redirectTarget", "supportsSelectedTeamLogin", "uuid"}}
        if len(query) != len(pairs) or set(query) - allowed[provider]:
            return None
        # Claude's `code=true` is a request flag, not a returned OAuth code.
        if provider == "claude" and (query.get("code", "true") != "true"
                or query.get("response_type", "code") != "code"
                or query.get("code_challenge_method", "S256") != "S256"
                or query.get("redirect_uri", "https://platform.claude.com/oauth/code/callback") != "https://platform.claude.com/oauth/code/callback"):
            return None
        if provider == "cursor" and (query.get("redirectTarget", "cli") != "cli" or query.get("mode", "login") != "login"
                or query.get("supportsSelectedTeamLogin", "true") != "true"):
            return None
        return value
    except ValueError:
        return None


def valid_input(value):
    return isinstance(value, str) and 8 <= len(value) <= MAX_CODE and re.fullmatch(r"[A-Za-z0-9_.~+/=#-]+", value) is not None


def provider_status(provider, installed=True, status="unknown", auth_method="unknown"):
    return {"id": provider, "label": PROVIDERS[provider], "installed": installed,
            "status": status, "auth_method": auth_method}


def parse_status(provider, returncode, output, environment=None):
    """Drop emails, directories, key suffixes, analytics and native error text."""
    result = provider_status(provider)
    text = output.decode(errors="replace") if isinstance(output, bytes) else output
    try:
        if provider == "codex":
            if returncode == 0 and re.search(r"\bLogged in using ChatGPT\b", text, re.I):
                result.update(status="connected", auth_method="subscription")
            elif returncode == 0 and re.search(r"\bLogged in using an? API key\b", text, re.I):
                result.update(status="connected", auth_method="api_key")
            elif re.search(r"\bNot logged in\b", text, re.I):
                result.update(status="disconnected", auth_method="none")
        else:
            value = json.loads(text)
            if not isinstance(value, dict):
                return result
            if provider == "claude":
                logged = value.get("loggedIn")
                method = value.get("authMethod")
                if logged is False:
                    result.update(status="disconnected", auth_method="none")
                elif logged is True and returncode == 0:
                    if method == "claude.ai":
                        result.update(status="connected", auth_method="subscription")
                    elif method in {"api_key", "apiKey"}:
                        result.update(status="connected", auth_method="api_key")
                    else:
                        result.update(status="connected", auth_method="unknown")
            elif provider == "cursor":
                if value.get("isAuthenticated") is False:
                    result.update(status="disconnected", auth_method="none")
                elif value.get("isAuthenticated") is True and returncode == 0:
                    # The native account check is not a subscription entitlement check.
                    result.update(status="connected", auth_method="account")
    except (ValueError, TypeError):
        return result
    overrides = os.environ if environment is None else environment
    if result["auth_method"] in {"subscription", "account"} and any(overrides.get(k) for k in OVERRIDES[provider]):
        result.update(status="unknown", auth_method="unknown")
        result["detail"] = "A provider environment override is set; billing mode cannot be confirmed."
    return result


def stop_group(process):
    if process is None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    except PermissionError:
        # macOS can report EPERM for a disappearing/reaped process group.
        # Do not lose the final protocol result after its leader is gone.
        if process.poll() is None:
            raise
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        pass
    # Also terminate descendants if the group leader exited before its children.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        if process.poll() is None:
            raise
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        pass


def capture(command, environment=None, timeout=10, cancelled=None):
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               start_new_session=True, env=environment)
    chunks, length, deadline = [], 0, time.monotonic() + timeout
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    try:
        while selector.get_map():
            if cancelled is not None and cancelled():
                raise InterruptedError
            if time.monotonic() >= deadline:
                raise TimeoutError
            for key, _ in selector.select(min(.2, max(0, deadline - time.monotonic()))):
                chunk = os.read(key.fd, 4096)
                if not chunk:
                    selector.unregister(key.fileobj)
                    continue
                length += len(chunk)
                if length > MAX_OUTPUT:
                    raise ValueError("Native output exceeded limit")
                chunks.append(chunk)
        while process.poll() is None:
            if cancelled is not None and cancelled():
                raise InterruptedError
            if time.monotonic() >= deadline:
                raise TimeoutError
            time.sleep(.05)
        return process.returncode, b"".join(chunks)
    finally:
        selector.close()
        stop_group(process)
        process.stdout.close()


def probe(provider, environment=None, cancelled=None):
    environment = dict(os.environ if environment is None else environment)
    command = COMMANDS[provider]
    executable = shutil.which(command["binary"], path=environment.get("PATH"))
    if executable is None:
        return provider_status(provider, False, "unavailable", "none")
    try:
        returncode, output = capture([executable, *command["status"]], environment, timeout=10, cancelled=cancelled)
        return parse_status(provider, returncode, output, environment)
    except (OSError, TimeoutError, subprocess.SubprocessError, ValueError):
        return provider_status(provider)


class LoginOutput:
    def __init__(self, provider):
        self.provider, self.buffer, self.url, self.code, self.input_required = provider, "", None, None, False
        self.terminal_state = "text"

    def _visible(self, text):
        # Parse controls before bounding the visible buffer. A partial/oversized
        # OSC payload must never become visible when its opening marker falls
        # outside a rolling buffer. OSC8 link targets and OSC52 clipboard data
        # are discarded; ordinary text between opening/closing links survives.
        output = []
        for char in text:
            state = self.terminal_state
            if state == "osc":
                if char in {"\x07", "\x9c"}:
                    self.terminal_state = "text"
                elif char == "\x1b":
                    self.terminal_state = "osc_escape"
            elif state == "osc_escape":
                if char in {"\\", "\x07", "\x9c"}:
                    self.terminal_state = "text"
                elif char != "\x1b":
                    self.terminal_state = "osc"
            elif state == "escape":
                self.terminal_state = {"[": "csi", "]": "osc"}.get(char, "text")
            elif state == "csi":
                if "@" <= char <= "~":
                    self.terminal_state = "text"
            elif char in {"\x1b", "\x9b", "\x9d"}:
                self.terminal_state = {"\x1b": "escape", "\x9b": "csi", "\x9d": "osc"}[char]
            elif (ord(char) >= 32 and not 127 <= ord(char) <= 159) or char in {"\n", "\t"}:
                output.append(char)
        return "".join(output)

    def feed(self, data):
        text = data.decode(errors="replace") if isinstance(data, bytes) else data
        self.buffer = (self.buffer + self._visible(text))[-16384:]
        clean = self.buffer
        events = []
        for match in re.finditer(r"https://[^\s<>\"']+(?=[\s<>\"'])", clean):
            value = authorization_url(self.provider, match[0])
            if value and value != self.url:
                self.url = value
                events.append({"type": "authorization", "authorization_url": value})
        if self.provider == "codex" and self.url:
            match = re.search(r"(?<![A-Z0-9])[A-Z0-9]{4,8}-[A-Z0-9]{4,8}(?![A-Z0-9])", clean)
            if match and match[0] != self.code:
                self.code = match[0]
                events.append({"type": "code", "user_code": self.code})
        if self.provider == "claude" and self.url and not self.input_required and re.search(
                r"paste (?:the )?(?:authentication )?code|enter (?:the )?authorization code|"
                # setup-token uses cursor movements for spacing, which leaves
                # this compact prompt after terminal controls are discarded.
                r"paste\s*code\s*here\s*if\s*prompted\s*>", clean, re.I):
            self.input_required = True
            events.append({"type": "input_required"})
        return events


def emit(value):
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def provider_lock(provider):
    directory = Path.home() / ".local" / "state" / "enginebox"
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("Unsafe auth lock directory")
    fd = os.open(directory / ("auth-" + provider + ".lock"), os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
            raise ValueError("Unsafe auth lock")
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return fd
    except BaseException:
        os.close(fd)
        raise


def login(provider, seconds=MAX_SECONDS, *, login_args=None, output_hook=None, completion_hook=None):
    lock, process, master, slave, selector = None, None, None, None, None
    interrupted = False
    def interrupt(_signum, _frame):
        nonlocal interrupted
        interrupted = True
    for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(signum, interrupt)
    result = {"type": "result", "reason": "failed"}
    try:
        lock = provider_lock(provider)
        environment = subscription_environment(provider)
        executable = shutil.which(COMMANDS[provider]["binary"], path=environment.get("PATH"))
        if executable is None:
            result["reason"] = "unavailable"
            return
        master, slave = pty.openpty()
        attributes = termios.tcgetattr(slave)
        attributes[3] &= ~termios.ECHO
        termios.tcsetattr(slave, termios.TCSANOW, attributes)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 4096, 0, 0))
        process = subprocess.Popen([executable, *(COMMANDS[provider]["login"] if login_args is None else login_args)], stdin=slave, stdout=slave,
                                   stderr=slave, env=environment, start_new_session=True)
        os.close(slave)
        slave = None
        selector = selectors.DefaultSelector()
        selector.register(master, selectors.EVENT_READ, "output")
        selector.register(sys.stdin.fileno(), selectors.EVENT_READ, "control")
        deadline, control = time.monotonic() + min(MAX_SECONDS, max(1, seconds)), b""
        parser = LoginOutput(provider)
        def consume_output(data):
            for event in parser.feed(data):
                emit(event)
            if output_hook is not None:
                output_hook(parser.buffer)
        emit({"type": "started"})
        while True:
            if interrupted:
                result["reason"] = "cancelled"
                break
            if time.monotonic() >= deadline:
                result["reason"] = "expired"
                break
            if process.poll() is not None:
                stop_group(process)
                # Native setup-token may print its result immediately before
                # exiting. Drain the PTY before verifying the completed login.
                os.set_blocking(master, False)
                while True:
                    try:
                        remaining = os.read(master, 4096)
                        if not remaining:
                            break
                        consume_output(remaining)
                    except (BlockingIOError, OSError):
                        break
                # Only native status after success can establish a connected account.
                if process.returncode == 0:
                    def cancelled_verification():
                        nonlocal interrupted
                        if time.monotonic() >= deadline:
                            result["reason"] = "expired"
                            interrupted = True
                        if not interrupted and select.select([sys.stdin.fileno()], [], [], 0)[0]:
                            # Login has exited: no further code input is expected.
                            # EOF or any control message cancels verification.
                            os.read(sys.stdin.fileno(), 4096)
                            interrupted = True
                        return interrupted
                    status = (completion_hook(environment, cancelled_verification) if completion_hook else
                              probe(provider, environment, cancelled=cancelled_verification))
                    if interrupted:
                        if result["reason"] != "expired":
                            result["reason"] = "cancelled"
                    elif status["status"] == "connected" and status["auth_method"] in {"subscription", "account"}:
                        result.update(reason="completed", account=status)
                    else:
                        result.update(reason="unverified", account=status)
                break
            for key, _ in selector.select(.2):
                try:
                    data = os.read(key.fd, 4096)
                except OSError as exc:
                    if key.data == "output" and exc.errno == errno.EIO:
                        selector.unregister(key.fd)
                        continue
                    raise
                if key.data == "output":
                    if not data:
                        selector.unregister(key.fd)
                    else:
                        consume_output(data)
                else:
                    if not data:
                        interrupted = True
                        break
                    control += data
                    if len(control) > 8192:
                        interrupted = True
                        break
                    while b"\n" in control:
                        line, control = control.split(b"\n", 1)
                        try:
                            message = json.loads(line)
                        except ValueError:
                            interrupted = True
                            break
                        if not isinstance(message, dict) or message.get("type") == "cancel":
                            interrupted = True
                            break
                        if (set(message) == {"type", "text"} and message["type"] == "input" and provider == "claude"
                                and parser.input_required and valid_input(message["text"])):
                            os.write(master, (message["text"] + "\n").encode())
                            parser.input_required = False
                            parser.buffer = ""
                            # Never echo, emit or persist the submitted code.
                            emit({"type": "input_accepted"})
                        else:
                            interrupted = True
                            break
                        message.clear()
    except BlockingIOError:
        result["reason"] = "busy"
    except (OSError, ValueError, subprocess.SubprocessError):
        result["reason"] = "failed"
    finally:
        stop_group(process)
        if selector is not None:
            selector.close()
        for descriptor in (master, slave, lock):
            if descriptor is not None:
                os.close(descriptor)
        emit(result)


def main():
    if len(sys.argv) < 3 or sys.argv[1] not in {"status", "login"}:
        raise SystemExit(2)
    mode, provider = sys.argv[1:3]
    if mode == "status" and provider == "all":
        with ThreadPoolExecutor(max_workers=3) as executor:
            emit({"type": "statuses", "providers": list(executor.map(probe, PROVIDERS))})
    elif provider in PROVIDERS and mode == "status":
        emit({"type": "statuses", "providers": [probe(provider)]})
    elif provider in PROVIDERS:
        login(provider)
    else:
        raise SystemExit(2)


if __name__ == "__main__":
    main()
