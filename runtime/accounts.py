"""Ephemeral native account connections inside guarded private workspaces."""
from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import threading
import time
import uuid

import guest_auth


ACTIVE = {"starting", "waiting"}
TERMINAL = {"succeeded", "failed", "cancelled", "expired"}
MAX_SESSIONS = 16
SESSION_SECONDS = 600
PUBLIC_KEYS = {"id", "host_id", "name", "provider", "status", "authorization_url", "user_code",
               "input_required", "message", "expires_at"}
METHODS = {"subscription", "account", "api_key", "unknown", "none"}
PROVIDER_STATES = {"connected", "disconnected", "unknown", "unavailable"}


def _provider(provider):
    if not isinstance(provider, str) or provider not in guest_auth.PROVIDERS:
        raise ValueError("Unknown native account provider.")
    return provider


def guest_command(host, name, mode, provider):
    """No shell, operator login environment or caller-provided command strings."""
    box = host.box()
    guest = box.guest
    source = Path(guest_auth.__file__).read_text()
    path = guest.home + "/.local/bin:/usr/local/bin:/usr/bin:/bin"
    command = box.command("exec", name, "--mode=non-interactive", f"--user={guest.uid}", f"--group={guest.gid}",
                          f"--cwd={guest.workspace}", f"--env=HOME={guest.home}", f"--env=USER={guest.user}",
                          f"--env=LOGNAME={guest.user}", f"--env=PATH={path}", "--", "python3", "-I", "-u", "-c", source, mode, provider)
    if command[0] == "ssh":
        command[1:1] = ["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=2"]
    return command


def _safe_provider(value, provider):
    result = guest_auth.provider_status(provider, False)
    if not isinstance(value, dict) or value.get("id") != provider:
        return result
    if (type(value.get("installed")) is bool and isinstance(value.get("status"), str)
            and value["status"] in PROVIDER_STATES and isinstance(value.get("auth_method"), str)
            and value["auth_method"] in METHODS):
        result.update(installed=value["installed"], status=value["status"], auth_method=value["auth_method"])
        if value.get("detail") == "A provider environment override is set; billing mode cannot be confirmed.":
            result["detail"] = value["detail"]
    return result


@dataclass
class _Session:
    public: dict
    created: float = field(default_factory=time.monotonic)
    process: object = None
    thread: object = None
    stop: threading.Event = field(default_factory=threading.Event)
    stop_reason: str = "cancelled"
    input_pending: bool = False
    io_lock: threading.Lock = field(default_factory=threading.Lock)


class AccountManager:
    def __init__(self, fleet, *, command_factory=None):
        self.fleet = fleet
        self.command_factory = command_factory
        self._lock = threading.RLock()
        self._sessions = {}
        self._closed = False
        self._probes = threading.BoundedSemaphore(4)
        self._cache = {}

    def _snapshot(self, session):
        return {key: value for key, value in session.public.items() if key in PUBLIC_KEYS}

    def _find(self, host_id, name, provider, session_id):
        _provider(provider)
        if not isinstance(session_id, str) or not re.fullmatch(r"[a-f0-9]{32}", session_id):
            raise ValueError("Unknown account connection session.")
        value = self._sessions.get(session_id)
        if value is None or (value.public["host_id"], value.public["name"], value.public["provider"]) != (host_id, name, provider):
            raise ValueError("Unknown account connection session.")
        return value

    def active(self, host_id, name):
        with self._lock:
            return any(s.public["host_id"] == host_id and s.public["name"] == name and s.public["status"] in ACTIVE
                       for s in self._sessions.values())

    def status(self, host_id, name):
        host = self.fleet.account_workspace(host_id, name)
        key = (host_id, name)
        with self._lock:
            if self._closed:
                raise ValueError("Account connections are shutting down.")
            cached = self._cache.get(key)
        if cached and time.monotonic() - cached[0] < 2:
            providers = [dict(item) for item in cached[1]]
        else:
            if not self._probes.acquire(blocking=False):
                raise ValueError("Account status checks are busy; retry shortly.")
            try:
                providers = [guest_auth.provider_status(p, False) for p in guest_auth.PROVIDERS]
                try:
                    command = (self.command_factory or guest_command)(host, name, "status", "all")
                    _, output = guest_auth.capture(command, timeout=25)
                    for line in output.splitlines():
                        try:
                            event = json.loads(line)
                        except ValueError:
                            continue
                        if isinstance(event, dict) and event.get("type") == "statuses" and isinstance(event.get("providers"), list):
                            supplied = {p.get("id"): p for p in event["providers"] if isinstance(p, dict)}
                            providers = [_safe_provider(supplied.get(p), p) for p in guest_auth.PROVIDERS]
                except (OSError, ValueError, TimeoutError, subprocess.SubprocessError):
                    pass
                with self._lock:
                    self._cache[key] = (time.monotonic(), [dict(item) for item in providers])
                    # Status polling cannot grow memory without bound.
                    while len(self._cache) > 64:
                        self._cache.pop(next(iter(self._cache)))
            finally:
                self._probes.release()
        with self._lock:
            for provider in providers:
                matching = [s for s in self._sessions.values() if
                            (s.public["host_id"], s.public["name"], s.public["provider"]) == (host_id, name, provider["id"])]
                if matching:
                    provider["session"] = self._snapshot(max(matching, key=lambda s: s.created))
        return {"providers": providers}

    def start(self, host_id, name, provider):
        _provider(provider)
        with self._lock:
            if self._closed:
                raise ValueError("Account connections are shutting down.")
            for value in self._sessions.values():
                if (value.public["host_id"], value.public["name"], value.public["provider"]) == (host_id, name, provider) and value.public["status"] in ACTIVE:
                    return self._snapshot(value)
            while len(self._sessions) >= MAX_SESSIONS:
                terminal = [s for s in self._sessions.values() if s.public["status"] in TERMINAL]
                if not terminal:
                    raise ValueError("Too many native account connections are active.")
                oldest = min(terminal, key=lambda s: s.created)
                del self._sessions[oldest.public["id"]]
            session_id = uuid.uuid4().hex
            value = _Session({"id": session_id, "host_id": host_id, "name": name, "provider": provider,
                              "status": "starting", "input_required": False,
                              "message": "Starting native sign-in in this workspace.",
                              "expires_at": (datetime.now(timezone.utc) + timedelta(seconds=SESSION_SECONDS)).isoformat()})
            self._sessions[session_id] = value
        try:
            host = self.fleet.account_workspace(host_id, name)
            thread = threading.Thread(target=self._run, args=(host, value), name="enginebox-account-" + session_id[:8], daemon=True)
            with self._lock:
                value.thread = thread
                if self._closed:
                    raise ValueError("Account connections are shutting down.")
                thread.start()
                return self._snapshot(value)
        except BaseException:
            with self._lock:
                self._sessions.pop(session_id, None)
            raise

    def session(self, host_id, name, provider, session_id):
        with self._lock:
            return self._snapshot(self._find(host_id, name, provider, session_id))

    def input(self, host_id, name, provider, session_id, text):
        if provider != "claude" or not guest_auth.valid_input(text):
            raise ValueError("Only a bounded Claude authorization code is accepted; paste the code without whitespace.")
        self.fleet.account_workspace(host_id, name)
        with self._lock:
            value = self._find(host_id, name, provider, session_id)
            if value.public["status"] != "waiting" or not value.public["input_required"] or value.input_pending:
                raise ValueError("This native sign-in is not waiting for an authorization code.")
            value.input_pending = True
            value.public.update(input_required=False, message="Checking the authorization code with the native client.")
        if not self._send(value, {"type": "input", "text": text}):
            with self._lock:
                value.input_pending = False
            raise ValueError("The native sign-in connection is unavailable; start a new connection.")
        with self._lock:
            return self._snapshot(value)

    def cancel(self, host_id, name, provider, session_id):
        with self._lock:
            value = self._find(host_id, name, provider, session_id)
            if value.public["status"] not in ACTIVE:
                return self._snapshot(value)
            value.stop.set()
        self._send(value, {"type": "cancel"})
        if value.thread is not None:
            value.thread.join(timeout=4)
        with self._lock:
            return self._snapshot(value)

    @staticmethod
    def _send(value, message):
        with value.io_lock:
            process = value.process
            if process is None or process.stdin is None or process.poll() is not None:
                return False
            try:
                process.stdin.write(json.dumps(message, separators=(",", ":")).encode() + b"\n")
                process.stdin.flush()
                return True
            except (OSError, ValueError):
                return False

    def _finish(self, value, state, message):
        with self._lock:
            if value.public["status"] in TERMINAL:
                return
            value.public.update(status=state, input_required=False, message=message)
            # Do not retain browser challenges/codes after completion/cancellation.
            value.public.pop("authorization_url", None)
            value.public.pop("user_code", None)
            value.input_pending = False
            self._cache.pop((value.public["host_id"], value.public["name"]), None)

    def _event(self, value, event):
        if not isinstance(event, dict):
            return
        provider, kind = value.public["provider"], event.get("type")
        with self._lock:
            if value.public["status"] not in ACTIVE:
                return
            if kind == "started":
                value.public.update(status="waiting", message="Waiting for the native client's sign-in instructions.")
            elif kind == "authorization":
                url = guest_auth.authorization_url(provider, event.get("authorization_url"))
                if url:
                    value.public.update(status="waiting", authorization_url=url,
                                        message="Open the provider's sign-in page to connect this workspace.")
            elif kind == "code" and provider == "codex":
                code = event.get("user_code")
                if isinstance(code, str) and re.fullmatch(r"[A-Z0-9]{4,8}-[A-Z0-9]{4,8}", code):
                    value.public.update(user_code=code, message="Enter this one-time code on the provider's sign-in page.")
            elif kind == "input_required" and provider == "claude" and value.public.get("authorization_url"):
                value.public.update(status="waiting", input_required=True,
                                    message="Complete sign-in in your browser, then paste the returned authorization code.")
            elif kind == "input_accepted":
                value.input_pending = False
                value.public.update(input_required=False, message="Waiting for the native client to finish sign-in.")
            elif kind == "result":
                reason = event.get("reason")
                if value.stop.is_set():
                    self._finish(value, value.stop_reason, "Native sign-in expired." if value.stop_reason == "expired" else "Native sign-in cancelled.")
                elif reason == "completed":
                    account = _safe_provider(event.get("account"), provider)
                    if account["status"] == "connected" and account["auth_method"] in {"subscription", "account"}:
                        self._finish(value, "succeeded", "Native account connection confirmed in this workspace.")
                    else:
                        self._finish(value, "failed", "The native client could not confirm an account connection.")
                elif reason in {"cancelled", "expired"}:
                    self._finish(value, reason, "Native sign-in cancelled." if reason == "cancelled" else "Native sign-in expired.")
                else:
                    messages = {"busy": "A native sign-in is already running for this provider in the workspace.",
                                "unavailable": "The native client is not installed in this workspace.",
                                "unverified": "The native client finished but did not confirm an account connection."}
                    self._finish(value, "failed", messages.get(reason, "Native sign-in failed. Retry or use the native terminal login."))

    def _run(self, host, value):
        process, selector, buffer, cancellation_deadline = None, None, b"", None
        try:
            command = (self.command_factory or guest_command)(host, value.public["name"], "login", value.public["provider"])
            process = subprocess.Popen(command,
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                       start_new_session=True, bufsize=0)
            with self._lock:
                value.process = process
            selector = selectors.DefaultSelector()
            selector.register(process.stdout, selectors.EVENT_READ)
            while selector.get_map():
                if time.monotonic() - value.created >= SESSION_SECONDS:
                    value.stop_reason = "expired"
                    value.stop.set()
                if value.stop.is_set() and cancellation_deadline is None:
                    self._send(value, {"type": "cancel"})
                    cancellation_deadline = time.monotonic() + 3
                if cancellation_deadline is not None and time.monotonic() >= cancellation_deadline:
                    break
                for key, _ in selector.select(.2):
                    chunk = os.read(key.fd, 4096)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    buffer += chunk
                    if len(buffer) > 16384:
                        raise ValueError("Invalid supervisor protocol")
                    while b"\n" in buffer:
                        line, buffer = buffer.split(b"\n", 1)
                        try:
                            event = json.loads(line)
                        except ValueError:
                            continue
                        self._event(value, event)
            if process.poll() is None:
                try:
                    process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    pass
        except (OSError, ValueError, subprocess.SubprocessError):
            pass
        finally:
            if process is not None:
                # EOF makes the guest supervisor kill its native process group;
                # its independent deadline also handles a lost SSH connection.
                with value.io_lock:
                    if process.stdin is not None:
                        process.stdin.close()
                guest_auth.stop_group(process)
                process.stdout.close()
            if selector is not None:
                selector.close()
            self._finish(value, value.stop_reason if value.stop.is_set() else "failed",
                         "Native sign-in expired." if value.stop_reason == "expired" else
                         "Native sign-in cancelled." if value.stop.is_set() else
                         "The native sign-in connection ended without confirmed authentication.")

    def close(self):
        with self._lock:
            self._closed = True
            active = [s for s in self._sessions.values() if s.public["status"] in ACTIVE]
            for value in active:
                value.stop.set()
        for value in active:
            self._send(value, {"type": "cancel"})
        deadline = time.monotonic() + 5
        for value in active:
            if value.thread is not None:
                value.thread.join(timeout=max(0, deadline - time.monotonic()))
