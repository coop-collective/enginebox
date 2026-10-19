"""Native process/MCP integration with disposable executable fixtures; no inference."""
import asyncio
from contextlib import nullcontext
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from agent_runs import AgentRuns, bounded_process
from fleet import Fleet, Host
from fleet_api import handler_for
from guest_agent_run import execute, lease_active, MAX_OUTPUT, validate_request


FIXTURE = r'''
import json, os, pathlib, signal, sys, time
provider = pathlib.Path(sys.argv[0]).name
if sys.argv[1:] == ["--version"]:
    print({"codex":"codex-cli 0.163.0", "claude":"2.1.302 (Claude Code)", "agent":"2026.10.16-e3cb207"}[provider])
    sys.exit(0)
prompt = sys.argv[-1] if provider == "agent" else sys.stdin.read()
if prompt == "sleep": time.sleep(20)
if prompt == "orphan":
    pathlib.Path("native-child-pid").write_text(str(os.getpid()))
    time.sleep(20)
if prompt == "stubborn":
    if os.fork() == 0:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(2)
        pathlib.Path("survived-termination").write_text("bad")
        sys.exit(0)
    time.sleep(20)
if prompt == "overflow":
    print("x" * 500000, flush=True)
    time.sleep(20)
home = pathlib.Path(os.environ["HOME"])
native = home / {"codex": ".codex", "claude": ".claude", "agent": ".cursor"}[provider]
native.mkdir(exist_ok=True)
memory = native / "fixture-memory.json"
resume = sys.argv[sys.argv.index("resume") + 1] if "resume" in sys.argv else sys.argv[sys.argv.index("--resume") + 1] if "--resume" in sys.argv else None
session_id = {"codex":"11111111-1111-4111-8111-111111111111", "claude":"22222222-2222-4222-8222-222222222222", "agent":"33333333-3333-4333-8333-333333333333"}[provider]
if resume and (resume != session_id or not memory.exists()): sys.exit(9)
before = json.loads(memory.read_text()) if memory.exists() else None
memory.write_text(json.dumps({"remembered": prompt if before is None else before["remembered"]}))
scratch = pathlib.Path.cwd() / "scratchnotes.md"
scratch.write_text((scratch.read_text() if scratch.exists() else "") + provider + "\n")
if provider == "codex": print(json.dumps({"type":"thread.started", "thread_id": session_id}))
else: print(json.dumps({"type":"system", "session_id": session_id}))
print(json.dumps({"before": before, "cwd": str(pathlib.Path.cwd()), "argv": sys.argv[1:],
                  "ambient_secret": os.environ.get("ENGINEBOX_API_TOKEN"),
                  "ambient_api_key": os.environ.get("OPENAI_API_KEY"),
                  "ambient_codex_home": os.environ.get("CODEX_HOME")}))
'''


def prepare(root):
    home, workspace = root / "home", root / "workspace"
    binaries = home / ".local/bin"
    binaries.mkdir(parents=True, mode=0o700)
    workspace.mkdir()
    for name in ("codex", "claude", "agent"):
        path = binaries / name
        path.write_text("#!" + sys.executable + "\n" + FIXTURE)
        path.chmod(0o700)
    return home, workspace


class GuestAgentRunTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.home, self.workspace = prepare(Path(self.directory.name))

    def test_native_homes_memory_scratch_and_exact_resume_are_reused_but_other_homes_are_empty(self):
        other = Path(self.directory.name) / "other"
        other.mkdir()
        other_home, other_workspace = prepare(other)
        for provider in ("codex", "claude", "cursor"):
            with self.subTest(provider=provider), patch.dict(os.environ, {"ENGINEBOX_API_TOKEN": "private-fixture",
                       "OPENAI_API_KEY": "do-not-inherit", "CODEX_HOME": "/wrong-owner-home"}):
                first = execute({"provider": provider, "prompt": "canary-" + provider}, self.home, self.workspace)
                self.assertEqual(first["exit_code"], 0)
                event, details = map(json.loads, first["output"].splitlines())
                native_id = event.get("thread_id", event.get("session_id"))
                self.assertIsNone(details["before"])
                self.assertIsNone(details["ambient_secret"])
                self.assertIsNone(details["ambient_api_key"])
                self.assertIsNone(details["ambient_codex_home"])
                second = execute({"provider": provider, "prompt": "remember", "resume_id": native_id}, self.home, self.workspace)
                self.assertEqual(json.loads(second["output"].splitlines()[-1])["before"]["remembered"], "canary-" + provider)
                missing = execute({"provider": provider, "prompt": "remember", "resume_id": native_id}, other_home, other_workspace)
                self.assertNotEqual(missing["exit_code"], 0)
                separate = execute({"provider": provider, "prompt": "unrelated-owner"}, other_home, other_workspace)
                self.assertIsNone(json.loads(separate["output"].splitlines()[-1])["before"])
        self.assertEqual(len((self.workspace / "scratchnotes.md").read_text().splitlines()), 6)

    def test_timeout_and_output_limit_stop_the_native_process(self):
        started = time.monotonic()
        result = execute({"provider": "claude", "prompt": "sleep", "timeout_seconds": 1}, self.home, self.workspace)
        self.assertTrue(result["timed_out"])
        self.assertLess(time.monotonic() - started, 5)
        result = execute({"provider": "codex", "prompt": "overflow", "timeout_seconds": 5}, self.home, self.workspace)
        self.assertTrue(result["output_truncated"])
        self.assertEqual(len(result["output"]), MAX_OUTPUT)

    def test_termination_kills_descendants_even_after_group_leader_exits(self):
        result = execute({"provider": "claude", "prompt": "stubborn", "timeout_seconds": 1}, self.home, self.workspace)
        self.assertTrue(result["timed_out"])
        time.sleep(1.4)
        self.assertFalse((self.workspace / "survived-termination").exists())

    def test_native_child_retains_writer_lease_if_supervisor_is_force_killed(self):
        source = "from guest_agent_run import execute; import sys; execute({'provider':'claude','prompt':'orphan','timeout_seconds':10}, sys.argv[1], sys.argv[2])"
        process = subprocess.Popen([sys.executable, "-c", source, str(self.home), str(self.workspace)],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        child_pid = None
        try:
            deadline = time.monotonic() + 5
            marker = self.workspace / "native-child-pid"
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertTrue(marker.exists())
            child_pid = int(marker.read_text())
            self.assertTrue(lease_active(self.home, "claude"))
            process.kill()
            process.wait(timeout=3)
            self.assertTrue(lease_active(self.home, "claude"))
            with self.assertRaisesRegex(ValueError, "active native run"):
                execute({"provider": "claude", "prompt": "hello"}, self.home, self.workspace)
        finally:
            if child_pid:
                try:
                    os.killpg(child_pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            if process.poll() is None:
                process.kill()
            process.wait(timeout=3)
        deadline = time.monotonic() + 2
        while lease_active(self.home, "claude") and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertFalse(lease_active(self.home, "claude"))

    def test_path_escape_and_symlink_state_are_rejected(self):
        (self.workspace / "escape").symlink_to(self.home, target_is_directory=True)
        with self.assertRaises(ValueError):
            execute({"provider": "codex", "prompt": "hello", "workdir": "escape"}, self.home, self.workspace)
        (self.home / ".local/state").symlink_to(self.workspace, target_is_directory=True)
        with self.assertRaises(ValueError):
            execute({"provider": "codex", "prompt": "hello"}, self.home, self.workspace)

    def test_same_provider_lease_rejects_concurrent_process_writers(self):
        result = []
        thread = threading.Thread(target=lambda: result.append(execute(
            {"provider": "claude", "prompt": "sleep", "timeout_seconds": 1}, self.home, self.workspace)))
        thread.start()
        try:
            deadline = time.monotonic() + 1
            while not (self.home / ".local/state/enginebox/agent-runs/claude.lock").exists() and time.monotonic() < deadline:
                time.sleep(.01)
            time.sleep(.05)
            with self.assertRaisesRegex(ValueError, "active native run"):
                execute({"provider": "claude", "prompt": "hello"}, self.home, self.workspace)
        finally:
            thread.join(timeout=4)
        self.assertTrue(result[0]["timed_out"])

    def test_request_cannot_override_tenant_home_commands_or_resume_latest(self):
        for change in [{"tenant": "other"}, {"home": "/elsewhere"}, {"command": "sh"},
                       {"provider": "shell"}, {"workdir": "../../other"}, {"workdir": "/other"},
                       {"resume_id": "--last"}, {"resume_id": "latest"}, {"model": "--bad"},
                       {"resume_id": "named-conversation"},
                       {"timeout_seconds": True}, {"timeout_seconds": 301}]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                validate_request({"provider": "codex", "prompt": "hello", **change})

    def test_unrelated_agent_binary_is_not_launched_as_cursor(self):
        (self.home / ".local/bin/agent").write_text("#!" + sys.executable + "\nprint('grok 1.0.40')\n")
        with self.assertRaisesRegex(ValueError, "expected native client"):
            execute({"provider": "cursor", "prompt": "hello"}, self.home, self.workspace)

    def test_controller_transport_bounds_bytes_before_buffering_and_times_out(self):
        with self.assertRaisesRegex(ValueError, "output bound"):
            bounded_process([sys.executable, "-c", "print('x'*1000000)"], b"", 2, 128)
        with self.assertRaisesRegex(ValueError, "timed out"):
            bounded_process([sys.executable, "-c", "import time; time.sleep(10)"], b"", .1, 128)

    def test_operator_mcp_does_not_import_posix_only_controller_modules(self):
        source = """import builtins, sys
# The installed SDK selects its own POSIX implementation on this test host;
# preload it so this check targets Engine Box's portable import dependency graph.
import mcp.server
original = builtins.__import__
def guarded(name, *args, **kwargs):
    if name in {'fcntl', 'pty', 'termios'}: raise ImportError('Windows fixture')
    return original(name, *args, **kwargs)
builtins.__import__ = guarded
import fleet_mcp
from guest_agent_run import validate_request
validate_request({'provider': 'claude', 'prompt': 'hello'})
print('portable imports')
"""
        code, output = bounded_process([sys.executable, "-c", source], b"", 10, 1024)
        self.assertEqual(code, 0)
        self.assertIn(b"portable imports", output)


class AgentRunProtocolTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.home, self.workspace = prepare(Path(self.directory.name))
        self.host = Host("primary", "Fixture", "fixture-not-real", "enginebox", "pool", 2, 4, 10, 30, 8, ())
        self.fleet = Fleet([self.host])
        self.addCleanup(self.fleet.executor.shutdown)
        self.addCleanup(self.fleet.accounts.close)
        self.addCleanup(self.fleet.agent_runs.close)
        self.fleet.agent_runs.root = Path(self.directory.name) / "private-records"
        self.fleet.agent_runs.root.mkdir(mode=0o700)
        self.fleet.agent_runs.runner = lambda host, name, request: execute(request, self.home, self.workspace)
        self.vm = {"name": "ebox-fixture", "project": "enginebox", "pool": "pool", "status": "Running", "managed": True}
        for fixture in (patch("fleet.inventory", side_effect=lambda _host: {"instances": [self.vm.copy()]}),
                        patch("fleet.host_operation_lock", side_effect=lambda _host: nullcontext())):
            fixture.start()
            self.addCleanup(fixture.stop)
        self.token = "fixture-only-token-" + "x" * 32
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(self.fleet, self.token))
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
        self.thread.start()
        self.addCleanup(self.close_server)
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        self.path = "/v1/hosts/primary/instances/ebox-fixture/agent-runs"

    def close_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def request(self, path, body=None, token=True, origin=False):
        headers = {"Content-Type": "application/json"}
        if token:
            headers["Authorization"] = "Bearer " + self.token
        if origin:
            headers["Origin"] = "https://unrelated.example"
        try:
            response = urlopen(Request(self.base + path, data=json.dumps(body).encode() if body is not None else None,
                                       headers=headers), timeout=3)
        except HTTPError as error:
            response = error
        with response:
            return response.status, json.load(response)

    def wait_run(self, run_id):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            run = self.fleet.agent_runs.get("primary", "ebox-fixture", run_id)
            if run["status"] not in {"queued", "running"}:
                return run
            time.sleep(.02)
        self.fail("fixture native run did not finish")

    def test_auth_admission_workspace_scoping_and_private_records(self):
        body = {"provider": "claude", "prompt": "private-fixture-prompt"}
        self.assertEqual(self.request(self.path, body, token=False)[0], 401)
        self.assertEqual(self.request(self.path, body, origin=True)[0], 403)
        for change in [{"managed": False}, {"warm_pool_state": "ready"}, {"pool": "other"}, {"status": "Stopped"}]:
            original = self.vm.copy()
            self.vm.update(change)
            self.assertEqual(self.request(self.path, body)[0], 400)
            self.vm = original
        status, data = self.request(self.path, body)
        self.assertEqual(status, 202)
        self.assertNotIn("prompt", data["run"])
        run_id = data["run"]["id"]
        self.assertEqual(self.wait_run(run_id)["status"], "succeeded")
        self.assertEqual(self.request(self.path + "/" + run_id, token=False)[0], 401)
        self.assertEqual(self.request(self.path.replace("ebox-fixture", "ebox-other") + "/" + run_id)[0], 404)
        self.assertEqual((self.fleet.agent_runs.root / (run_id + ".json")).stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.fleet.jobs, {})

    def test_active_run_prevents_same_provider_and_lifecycle_but_not_other_provider(self):
        _, data = self.request(self.path, {"provider": "claude", "prompt": "sleep", "timeout_seconds": 1})
        self.assertEqual(self.request(self.path, {"provider": "claude", "prompt": "hello"})[0], 400)
        with self.assertRaisesRegex(ValueError, "native run"):
            self.fleet._action(self.host, "ebox-fixture", "pause")
        self.assertEqual(self.request(self.path, {"provider": "codex", "prompt": "hello"})[0], 202)
        self.wait_run(data["run"]["id"])

    def test_restart_requires_read_only_guest_lease_check_before_lifecycle_or_relaunch(self):
        _, data = self.request(self.path, {"provider": "claude", "prompt": "sleep", "timeout_seconds": 1})
        deadline = time.monotonic() + 1
        while not lease_active(self.home, "claude") and time.monotonic() < deadline:
            time.sleep(.01)
        recovered = AgentRuns(self.fleet, self.fleet.agent_runs.root,
                              probe=lambda host, name, provider: lease_active(self.home, provider))
        self.addCleanup(recovered.close)
        self.assertEqual(recovered.get("primary", "ebox-fixture", data["run"]["id"])["status"], "failed")
        self.assertTrue(recovered.active("primary", "ebox-fixture"))
        with self.assertRaisesRegex(ValueError, "previous native run"):
            recovered.start("primary", "ebox-fixture", {"provider": "claude", "prompt": "hello"})
        self.wait_run(data["run"]["id"])
        self.assertFalse(recovered.active("primary", "ebox-fixture"))

    def test_recovered_records_reject_public_permissions_and_invalid_scope(self):
        _, data = self.request(self.path, {"provider": "codex", "prompt": "hello"})
        self.wait_run(data["run"]["id"])
        path = self.fleet.agent_runs.root / (data["run"]["id"] + ".json")
        path.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "Invalid native run record"):
            AgentRuns(self.fleet, self.fleet.agent_runs.root)
        path.chmod(0o600)
        value = json.loads(path.read_text())
        value["scope"] = "someone-else"
        path.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError, "Invalid native run record"):
            AgentRuns(self.fleet, self.fleet.agent_runs.root)

    def test_real_mcp_stdio_dispatches_all_native_harnesses_and_exact_resume(self):
        from mcp import Client, StdioServerParameters
        async def run():
            parameters = StdioServerParameters(command=sys.executable,
                args=[str(Path(__file__).resolve().parents[1] / "runtime/fleet_mcp.py")],
                env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "ENGINEBOX_API_URL": self.base,
                     "ENGINEBOX_API_TOKEN": self.token, "ENGINEBOX_GUEST": "0"})
            async with Client(parameters) as client:
                names = {tool.name for tool in (await client.list_tools()).tools}
                self.assertTrue({"agent_launch", "agent_run"} <= names)
                async def call(name, arguments):
                    result = await client.call_tool(name, arguments)
                    self.assertFalse(result.is_error, str(result))
                    return json.loads(next(block.text for block in result.content if block.type == "text"))
                for provider in ("codex", "claude", "cursor"):
                    native_id = None
                    for turn in range(2):
                        arguments = {"host_id": "primary", "name": "ebox-fixture", "provider": provider,
                                     "prompt": "first-canary" if turn == 0 else "remember"}
                        if native_id:
                            arguments["resume_id"] = native_id
                        launched = await call("agent_launch", arguments)
                        for _ in range(100):
                            result = (await call("agent_run", {"host_id": "primary", "name": "ebox-fixture",
                                                              "run_id": launched["run"]["id"]}))["run"]
                            if result["status"] not in {"queued", "running"}:
                                break
                            await asyncio.sleep(.02)
                        self.assertEqual(result["status"], "succeeded", result)
                        native_id = result["native_session_id"]
                        if turn:
                            self.assertEqual(json.loads(result["result"]["output"].splitlines()[-1])["before"],
                                             {"remembered": "first-canary"})
        asyncio.run(run())


if __name__ == "__main__":
    unittest.main()
