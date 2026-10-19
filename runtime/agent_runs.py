"""Single-owner native run admission, private records and exact-session resume."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import selectors
import stat
import subprocess
import tempfile
import threading
import time
import uuid

from enginebox import identifier
from guest_agent_run import MAX_OUTPUT, SESSION_ID, terminate, validate_request


def bounded_process(command, payload, timeout, maximum):
    """Bound output before buffering it, including an untrusted/misconfigured guest."""
    with tempfile.TemporaryFile() as input_file:
        input_file.write(payload)
        input_file.seek(0)
        process = subprocess.Popen(command, stdin=input_file, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL, start_new_session=True)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    output, deadline = bytearray(), time.monotonic() + timeout
    try:
        while selector.get_map():
            if time.monotonic() >= deadline:
                raise ValueError("Native transport timed out; inspect its guest lease.")
            for key, _ in selector.select(.1):
                chunk = os.read(key.fd, min(8192, maximum + 1 - len(output)))
                if not chunk:
                    selector.unregister(key.fileobj)
                else:
                    output.extend(chunk)
                    if len(output) > maximum:
                        raise ValueError("Native transport exceeded its output bound.")
        process.wait(timeout=max(.1, deadline - time.monotonic()))
        return process.returncode, bytes(output)
    finally:
        terminate(process)
        selector.close()
        process.stdout.close()


def validate_result(result):
    if (not isinstance(result, dict) or set(result) != {"exit_code", "output", "timed_out", "output_truncated"}
            or type(result["exit_code"]) is not int or not isinstance(result["output"], str)
            or len(result["output"].encode()) > MAX_OUTPUT * 3
            or type(result["timed_out"]) is not bool or type(result["output_truncated"]) is not bool):
        raise ValueError("Invalid native run response.")
    return result


def validate_record(value, run_id):
    required = {"id", "host_id", "name", "provider", "scope", "state_scope", "status", "workdir", "resume_id", "created_at"}
    allowed = required | {"finished_at", "result", "native_session_id", "error", "execution_uncertain"}
    if (not isinstance(value, dict) or not required <= set(value) or set(value) - allowed or value["id"] != run_id
            or value["scope"] != "controller-owner" or value["state_scope"] != "private-workspace"
            or value["status"] not in {"queued", "running", "failed", "succeeded"}
            or not isinstance(value["host_id"], str) or not isinstance(value["name"], str)):
        raise ValueError("Invalid native run record.")
    identifier(value["host_id"])
    identifier(value["name"], instance=True)
    validate_request({"provider": value["provider"], "prompt": "record-validation", "workdir": value["workdir"],
                      "resume_id": value["resume_id"]})
    for key in ("created_at", "finished_at"):
        if key in value and (not isinstance(value[key], str) or datetime.fromisoformat(value[key]).tzinfo is None):
            raise ValueError("Invalid native run timestamp.")
    if "native_session_id" in value:
        validate_request({"provider": value["provider"], "prompt": "record-validation", "resume_id": value["native_session_id"]})
    if "error" in value and (not isinstance(value["error"], str) or len(value["error"]) > 1024):
        raise ValueError("Invalid native run error.")
    if "execution_uncertain" in value and type(value["execution_uncertain"]) is not bool:
        raise ValueError("Invalid native run recovery state.")
    if "result" in value:
        validate_result(value["result"])
    return value


def native_session_id(output):
    for line in output.splitlines():
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if not isinstance(item, dict):
            continue
        value = item.get("thread_id") if item.get("type") == "thread.started" else item.get("session_id")
        if isinstance(value, str) and re.fullmatch(SESSION_ID, value):
            return value
    return None


def run_native(host, name, request):
    guest = host.guest
    source = Path(__file__).with_name("guest_agent_run.py").read_text()
    command = host.box().command("exec", name, "--mode=non-interactive",
                                f"--user={guest.uid}", f"--group={guest.gid}",
                                f"--cwd={guest.workspace}", f"--env=HOME={guest.home}",
                                f"--env=USER={guest.user}", f"--env=DISPLAY={guest.display}",
                                "--", "python3", "-I", "-c", source, guest.workspace)
    # Prompt and delegated credentials never enter the SSH/Incus argv or logs.
    code, output = bounded_process(command, json.dumps(request, ensure_ascii=False).encode(),
                                   request["timeout_seconds"] + 20, MAX_OUTPUT * 7)
    result = json.loads(output)
    if code or not isinstance(result, dict) or "error" in result:
        raise ValueError("Native launch failed; inspect installation and the workspace session lease.")
    return validate_result(result)


def probe_native(host, name, provider):
    source = Path(__file__).with_name("guest_agent_run.py").read_text()
    # Execute definitions without invoking the launch entrypoint, then probe the
    # same native-home lock. No tokens, model calls or workspace writes occur.
    source = source.rsplit('if __name__ == "__main__":', 1)[0]
    source += "\nprint(json.dumps({'active': lease_active(os.environ['HOME'], sys.argv[1])}))\n"
    command = host.box().command("exec", name, "--mode=non-interactive", f"--user={host.guest.uid}",
                                f"--group={host.guest.gid}", f"--env=HOME={host.guest.home}",
                                "--", "python3", "-I", "-c", source, provider)
    code, output = bounded_process(command, b"", 15, 1024)
    result = json.loads(output)
    if code or not isinstance(result, dict) or set(result) != {"active"} or type(result["active"]) is not bool:
        raise ValueError("Cannot verify the native session lease.")
    return result["active"]


class AgentRuns:
    def __init__(self, fleet, root=None, *, runner=None, probe=None):
        self.fleet, self.root = fleet, Path(root) if root else None
        self.runner = runner or run_native
        self.probe = probe or probe_native
        self.records, self.leases = {}, set()
        self.lock = threading.RLock()
        self.executor = ThreadPoolExecutor(max_workers=4, thread_name_prefix="enginebox-native")
        self.pending = threading.BoundedSemaphore(16)
        if self.root:
            self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
            info = self.root.lstat()
            if self.root.is_symlink() or info.st_uid != os.geteuid() or info.st_mode & 0o077:
                raise ValueError("Native run records require private controller storage.")
            for path in sorted(self.root.glob("*.json")):
                info = path.lstat()
                if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077
                        or info.st_nlink != 1 or not re.fullmatch(r"[a-f0-9]{32}\.json", path.name)
                        or info.st_size > MAX_OUTPUT * 7):
                    raise ValueError("Invalid native run record.")
                value = validate_record(json.loads(path.read_text()), path.stem)
                if value.get("status") in {"queued", "running"}:
                    value.update(status="failed", execution_uncertain=True,
                                 error="Controller restarted; its native session lease must be checked before another run or lifecycle change.")
                    self._save(value)
                self.records[value["id"]] = value

    def _save(self, value):
        if not self.root:
            return
        fd, temporary = tempfile.mkstemp(prefix=".run-", dir=self.root)
        try:
            with os.fdopen(fd, "w") as stream:
                json.dump(value, stream)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.root / (value["id"] + ".json"))
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)

    def active(self, host_id, name):
        with self.lock:
            if any(key[:2] == (host_id, name) for key in self.leases):
                return True
            uncertain = [value for value in self.records.values() if value.get("execution_uncertain")
                         and (value["host_id"], value["name"]) == (host_id, name)]
            for value in uncertain:
                try:
                    if self.probe(self.fleet.host(host_id), name, value["provider"]):
                        return True
                except Exception:
                    return True  # Fail closed if a paused/offline guest cannot be checked.
                value["execution_uncertain"] = False
                self._save(value)
            return False

    def get(self, host_id, name, run_id):
        identifier(host_id)
        identifier(name, instance=True)
        if not isinstance(run_id, str) or not re.fullmatch(r"[a-f0-9]{32}", run_id):
            raise ValueError("Invalid native run ID.")
        with self.lock:
            value = self.records.get(run_id)
            if not value or (value["host_id"], value["name"]) != (host_id, name):
                raise ValueError("Unknown native run for this workspace.")
            return json.loads(json.dumps(value))

    def start(self, host_id, name, request):
        request = validate_request(request)
        host = self.fleet.account_workspace(host_id, name)
        key = (host_id, name, request["provider"])
        with self.lock:
            if any(value.get("execution_uncertain") and (value["host_id"], value["name"]) == (host_id, name)
                   for value in self.records.values()) and self.active(host_id, name):
                raise ValueError("The previous native run must release its guest lease before another launch.")
            if key in self.leases:
                raise ValueError("This workspace provider already has an active native run.")
            if not self.pending.acquire(blocking=False):
                raise ValueError("Native run queue is full; wait for existing runs.")
            value = {"id": uuid.uuid4().hex, "host_id": host_id, "name": name, "provider": request["provider"],
                     "scope": "controller-owner", "state_scope": "private-workspace", "status": "queued",
                     "workdir": request["workdir"], "resume_id": request.get("resume_id"),
                     "created_at": datetime.now(timezone.utc).isoformat()}
            self.leases.add(key)
            self.records[value["id"]] = value
            try:
                self._save(value)
                self.executor.submit(self._work, host, name, request, value["id"], key)
            except Exception:
                self.leases.discard(key)
                self.pending.release()
                self.records.pop(value["id"], None)
                if self.root:
                    (self.root / (value["id"] + ".json")).unlink(missing_ok=True)
                raise
            return json.loads(json.dumps(value))

    def _work(self, host, name, request, run_id, key):
        outcome = {}
        try:
            with self.lock:
                self.records[run_id]["status"] = "running"
                self._save(self.records[run_id])
            result = self.runner(host, name, request)
            status = "succeeded" if result["exit_code"] == 0 and not result["timed_out"] and not result["output_truncated"] else "failed"
            outcome = {"status": status, "result": result,
                       "native_session_id": native_session_id(result["output"]) or request.get("resume_id")}
        except Exception:
            outcome = {"status": "failed", "execution_uncertain": True,
                       "error": "Native run failed. Check the installed client, connection, workspace and active session lease."}
        finally:
            with self.lock:
                self.records[run_id].update(outcome, finished_at=datetime.now(timezone.utc).isoformat())
                self.leases.discard(key)
                self.pending.release()
                self._save(self.records[run_id])
                # Cap private transcript storage; native session history remains
                # in the workspace and follows the provider's own retention.
                finished = [value for value in self.records.values() if value["status"] not in {"queued", "running"}
                            and not value.get("execution_uncertain")]
                for old in sorted(finished, key=lambda value: value["created_at"])[:-100]:
                    self.records.pop(old["id"], None)
                    if self.root:
                        (self.root / (old["id"] + ".json")).unlink(missing_ok=True)

    def close(self):
        self.executor.shutdown(wait=True)
