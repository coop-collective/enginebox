"""Account route isolation and workspace ownership; no provider or Incus calls."""
from contextlib import contextmanager
from http.server import ThreadingHTTPServer
import json
import threading
import unittest
from unittest.mock import Mock, patch
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from fleet import Fleet, Host
from fleet_api import handler_for


class AccountAPITests(unittest.TestCase):
    def setUp(self):
        self.fleet = Mock()
        self.session = {"id": "c" * 32, "host_id": "primary", "name": "ebox-work",
                        "provider": "claude", "status": "waiting", "input_required": True}
        self.agents = self.fleet.agent_access.return_value
        self.agents.start.return_value = self.session
        self.agents.session.return_value = self.session
        self.agents.input.return_value = self.session
        self.agents.cancel.return_value = self.session | {"status": "cancelled"}
        self.agents.status.return_value = {"providers": [], "scope": "controller"}
        self.token = "unit-test-token-only-" + "a" * 32
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(self.fleet, self.token))
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
        self.thread.start()
        self.addCleanup(self.close_server)
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        self.path = "/v1/agents"
        self.session_path = self.path + "/claude/sessions/" + self.session["id"]

    def close_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def request(self, path, body=None, *, authenticated=True, origin=None):
        headers = {"Content-Type": "application/json"}
        if authenticated:
            headers["Authorization"] = "Bearer " + self.token
        if origin:
            headers["Origin"] = origin
        request = Request(self.base + path, headers=headers,
                          data=None if body is None else json.dumps(body).encode())
        try:
            response = urlopen(request, timeout=2)
        except HTTPError as error:
            response = error
        with response:
            self.assertEqual(response.headers["Cache-Control"], "no-store")
            return response.status, json.load(response)

    def test_authentication_covers_status_start_poll_input_and_cancel(self):
        for path, body in [(self.path, None), (self.path + "/claude/login", {}),
                           (self.path + "/codex/usage", None),
                           (self.session_path, None), (self.session_path + "/input", {"text": "code"}),
                           (self.session_path + "/cancel", {})]:
            with self.subTest(path=path):
                self.assertEqual(self.request(path, body, authenticated=False)[0], 401)
                self.assertEqual(self.request(path, body, origin="https://outside.example")[0], 403)
        self.assertEqual(self.fleet.mock_calls, [])

    def test_usage_is_read_only_scoped_and_does_not_start_a_model_job(self):
        self.agents.usage.return_value = {"provider": "codex", "status": "available"}
        self.assertEqual(self.request(self.path + "/codex/usage"),
                         (200, {"usage": {"provider": "codex", "status": "available"}}))
        self.agents.usage.assert_called_once_with("codex")
        self.assertEqual(self.request(self.path + "/unknown/usage")[0], 404)
        self.assertEqual(self.request(self.path + "/codex/usage", {})[0], 404)
        self.agents.start.assert_not_called()
        self.fleet.submit.assert_not_called()

    def test_native_session_routes_are_scoped_and_do_not_create_job_records(self):
        self.assertEqual(self.request(self.path), (200, {"providers": [], "scope": "controller"}))
        self.assertEqual(self.request(self.path + "/claude/login", {})[0], 202)
        self.assertEqual(self.request(self.session_path)[0], 200)
        self.assertEqual(self.request(self.session_path + "/input", {"text": "one-time-code"})[0], 200)
        self.assertEqual(self.request(self.session_path + "/cancel", {})[1]["session"]["status"], "cancelled")
        self.agents.start.assert_called_once_with("claude")
        scope = ("claude", "c" * 32)
        self.agents.session.assert_called_once_with(*scope)
        self.agents.input.assert_called_once_with(*scope, "one-time-code")
        self.agents.cancel.assert_called_once_with(*scope)
        self.fleet.submit.assert_not_called()

    def test_workspace_login_redirects_to_controller_without_launching(self):
        status, body = self.request("/v1/hosts/primary/instances/ebox-work/accounts/claude/login", {})
        self.assertEqual(status, 409)
        self.assertIn("controller", body["error"])
        self.fleet.account_start.assert_not_called()

    def test_login_cannot_accept_commands_credentials_or_unknown_provider(self):
        for body in [{"command": "shell"}, {"api_key": "secret"}, {"password": "secret"}]:
            self.assertEqual(self.request(self.path + "/claude/login", body)[0], 400)
        self.assertEqual(self.request(self.path + "/arbitrary/login", {})[0], 404)
        self.assertEqual(self.request(self.session_path + "/input", {"text": ["code"]})[0], 400)
        self.assertEqual(self.request(self.session_path + "/input", {"text": "code", "command": "shell"})[0], 400)
        self.assertEqual(self.request(self.session_path + "/cancel", {"text": "code"})[0], 400)
        self.assertEqual(self.request(self.session_path + "/input", {"text": "x" * 17000})[0], 413)
        self.assertEqual(self.fleet.mock_calls, [])


class AccountWorkspaceTests(unittest.TestCase):
    def setUp(self):
        self.host = Host("primary", "Test host", "not-a-real-host", "enginebox", "pool", 2, 4, 10, 30, 8, ())
        self.fleet = Fleet([self.host])
        self.addCleanup(self.fleet.executor.shutdown)
        self.addCleanup(self.fleet.accounts.close)
        self.vm = {"name": "ebox-work", "project": "enginebox", "pool": "pool", "status": "Running", "managed": True}
        inventory = patch("fleet.inventory", side_effect=lambda _host: {"instances": [self.vm.copy()]})
        inventory.start()
        self.addCleanup(inventory.stop)

    def test_only_assigned_running_workspace_may_authenticate(self):
        self.assertIs(self.fleet.account_workspace("primary", "ebox-work"), self.host)
        for changes in [{"managed": False}, {"project": "unrelated"}, {"pool": "other"},
                        {"status": "Stopped"}, {"status": "Frozen"}, {"warm_pool_state": "ready"},
                        {"warm_pool_state": "preparing"}, {"warm_pool_state": "quarantined"}]:
            original = self.vm.copy()
            with self.subTest(changes=changes):
                self.vm.update(changes)
                with self.assertRaises(ValueError):
                    self.fleet.account_workspace("primary", "ebox-work")
            self.vm = original
        self.vm["warm_pool_state"] = "claimed"
        self.assertIs(self.fleet.account_workspace("primary", "ebox-work"), self.host)

    def test_active_sign_in_blocks_only_its_own_workspace_lifecycle(self):
        with patch.object(self.fleet.accounts, "active", side_effect=lambda host_id, name: name == "ebox-work"), \
                patch.object(Host, "box") as box:
            for action in ("pause", "hibernate", "stop"):
                with self.assertRaisesRegex(ValueError, "Finish or cancel"):
                    self.fleet._action(self.host, "ebox-work", action)
            box.assert_not_called()
            self.vm["name"] = "ebox-other"
            self.fleet._action(self.host, "ebox-other", "pause")
            box.return_value.lifecycle.assert_called_once_with("ebox-other", "pause")

    def test_login_admission_is_serialized_with_host_lifecycle(self):
        lock_active = []
        @contextmanager
        def host_lock(_host):
            lock_active.append(True)
            try:
                yield
            finally:
                lock_active.pop()
        def start(*_args):
            self.assertTrue(self.fleet.locks[self.host.id].locked())
            self.assertTrue(lock_active)
            return {"id": "1" * 32, "status": "starting"}
        with patch("fleet.host_operation_lock", side_effect=host_lock), patch.object(self.fleet.accounts, "start", side_effect=start):
            self.assertEqual(self.fleet.account_start("primary", "ebox-work", "codex")["status"], "starting")
        self.assertFalse(self.fleet.locks[self.host.id].locked())
        self.assertEqual(lock_active, [])


if __name__ == "__main__":
    unittest.main()
