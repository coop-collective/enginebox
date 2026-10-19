"""Native auth protocol/security tests. All clients are local test programs."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from accounts import AccountManager, _Session, guest_command
import accounts
from enginebox import EngineBox, GuestConfig
import guest_auth


CLAUDE_URL = "https://claude.com/cai/oauth/authorize?code=true&response_type=code&redirect_uri=https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback&state=example-state"
CURSOR_URL = "https://cursor.com/loginDeepControl?redirectTarget=cli&challenge=example-challenge&uuid=example-id"


class StatusAndProtocolTests(unittest.TestCase):
    def test_native_status_drops_keys_emails_and_paths(self):
        results = [guest_auth.parse_status("codex", 0, "Logged in using an API key - sk-secret-value", {}),
                   guest_auth.parse_status("claude", 0, json.dumps({"loggedIn": True, "authMethod": "claude.ai", "email": "private@example.com", "directory": "/private", "token": "secret"}), {}),
                   guest_auth.parse_status("cursor", 0, json.dumps({"isAuthenticated": True, "email": "private@example.com", "message": "secret"}), {})]
        self.assertEqual([r["auth_method"] for r in results], ["api_key", "subscription", "account"])
        output = json.dumps(results)
        for secret in ["sk-secret-value", "private@example.com", "/private", "secret"]:
            self.assertNotIn(secret, output)

    def test_exit_zero_does_not_establish_authentication(self):
        result = guest_auth.parse_status("cursor", 0, '{"status":"unauthenticated","isAuthenticated":false}')
        self.assertEqual(result["status"], "disconnected")
        self.assertEqual(guest_auth.parse_status("claude", 0, '{"loggedIn":false,"authMethod":"none"}')["status"], "disconnected")
        self.assertEqual(guest_auth.parse_status("claude", 1, '{"loggedIn":true,"authMethod":"claude.ai"}')["status"], "unknown")
        self.assertEqual(guest_auth.parse_status("codex", 0, "unknown result")["status"], "unknown")
        self.assertEqual(guest_auth.parse_status("claude", 0, '{"loggedIn":true,"authMethod":"oauth"}')["auth_method"], "unknown")

    def test_env_overrides_never_get_reported_as_confirmed_subscription(self):
        result = guest_auth.parse_status("codex", 0, "Logged in using ChatGPT", {"OPENAI_API_KEY": "sensitive"})
        self.assertEqual((result["status"], result["auth_method"]), ("unknown", "unknown"))
        environment = guest_auth.subscription_environment("claude", {"ANTHROPIC_API_KEY": "sensitive", "CURSOR_API_KEY": "sensitive", "OPENAI_BASE_URL": "https://proxy.example", "HOME": "/home/agent"})
        self.assertNotIn("ANTHROPIC_API_KEY", environment)
        self.assertNotIn("CURSOR_API_KEY", environment)
        self.assertNotIn("OPENAI_BASE_URL", environment)
        self.assertEqual(environment["NO_OPEN_BROWSER"], "1")
        self.assertEqual(environment["HOME"], "/home/agent")

    def test_links_are_exact_official_login_endpoints_not_callbacks_or_tokens(self):
        for provider, url in [("claude", CLAUDE_URL), ("cursor", CURSOR_URL), ("codex", "https://auth.openai.com/codex/device")]:
            self.assertEqual(guest_auth.authorization_url(provider, url), url)
        for url in ["http://auth.openai.com/codex/device", "https://auth.openai.com.evil.test/codex/device",
                    "https://user:pass@auth.openai.com/codex/device", "https://auth.openai.com:444/codex/device",
                    "https://auth.openai.com/codex/device?access_token=secret", "https://auth.openai.com/codex/device#secret",
                    "https://auth.openai.com/callback?code=secret"]:
            self.assertIsNone(guest_auth.authorization_url("codex", url))
        self.assertIsNone(guest_auth.authorization_url("claude", "https://platform.claude.com/oauth/code/callback?code=secret"))
        self.assertIsNone(guest_auth.authorization_url("claude", CLAUDE_URL.replace("code=true", "code=returned-secret")))
        self.assertIsNone(guest_auth.authorization_url("claude", CLAUDE_URL.replace("platform.claude.com", "evil.example")))
        self.assertIsNone(guest_auth.authorization_url("cursor", CURSOR_URL.replace("redirectTarget=cli", "redirectTarget=https://evil.example")))

    def test_split_output_never_publishes_partial_authorization_link(self):
        parser = guest_auth.LoginOutput("claude")
        self.assertEqual(parser.feed(CLAUDE_URL[:-8]), [])
        events = parser.feed(CLAUDE_URL[-8:] + "\nPaste code here: ")
        self.assertEqual(events[0], {"type": "authorization", "authorization_url": CLAUDE_URL})
        self.assertEqual(events[1], {"type": "input_required"})
        codex = guest_auth.LoginOutput("codex")
        events = codex.feed("https://auth.openai.com/codex/device\nEnter this one-time code\nABCD-EFGH\n")
        self.assertEqual(events[-1], {"type": "code", "user_code": "ABCD-EFGH"})
        self.assertFalse(codex.input_required)

    def test_osc8_hyperlinks_preserve_visible_url_for_bel_and_st(self):
        for terminator in ["\x07", "\x1b\\"]:
            with self.subTest(terminator=repr(terminator)):
                parser = guest_auth.LoginOutput("claude")
                output = "\x1b]8;;" + CLAUDE_URL + terminator + "\x1b[36m" + CLAUDE_URL + "\x1b[0m\x1b]8;;" + terminator + "\nPaste code here: "
                events = parser.feed(output.encode())
                self.assertEqual(events, [{"type": "authorization", "authorization_url": CLAUDE_URL},
                                          {"type": "input_required"}])
                self.assertNotIn("\x1b", parser.buffer)

    def test_compact_claude_setup_token_prompt_requires_verified_url_and_claude(self):
        prompt = "Paste\x1b[1Ccode\x1b[1Chere\x1b[1Cif\x1b[1Cprompted>"
        parser = guest_auth.LoginOutput("claude")
        self.assertEqual(parser.feed(CLAUDE_URL + "\n"), [{"type": "authorization", "authorization_url": CLAUDE_URL}])
        events = []
        for start in range(0, len(prompt), 3):
            events.extend(parser.feed(prompt[start:start + 3]))
        self.assertEqual(events, [{"type": "input_required"}])
        self.assertTrue(parser.input_required)
        self.assertEqual(parser.feed("Pastecodehereifprompted>"), [])
        for provider, url in [("claude", "https://example.com/login"), ("cursor", CURSOR_URL),
                              ("codex", "https://auth.openai.com/codex/device")]:
            parser = guest_auth.LoginOutput(provider)
            events = parser.feed(url + "\nPastecodehereifprompted>")
            self.assertNotIn({"type": "input_required"}, events)
            self.assertFalse(parser.input_required)

    def test_split_osc_sequences_never_publish_hidden_target(self):
        parser = guest_auth.LoginOutput("claude")
        hidden = CLAUDE_URL.replace("example-state", "hidden-state")
        chunks = ["\x1b", "]8;;" + hidden + "\n", "\x1b", "\\", CLAUDE_URL[:35], CLAUDE_URL[35:],
                  "\x1b]8;;\x1b", "\\\nPaste code here: "]
        events = []
        for index, chunk in enumerate(chunks):
            events.extend(parser.feed(chunk))
            if index < 7:
                self.assertEqual(events, [])
        self.assertEqual(events[0], {"type": "authorization", "authorization_url": CLAUDE_URL})
        self.assertEqual(events[-1], {"type": "input_required"})
        self.assertNotIn("hidden-state", json.dumps(events))

    def test_long_unterminated_clipboard_osc_cannot_escape_buffer_limit(self):
        parser = guest_auth.LoginOutput("claude")
        self.assertEqual(parser.feed("\x1b]52;c;" + "x" * 20000), [])
        self.assertEqual(parser.feed(CLAUDE_URL + "\nPaste code here: "), [])
        self.assertEqual(parser.buffer, "")
        events = parser.feed("\x07" + CLAUDE_URL + "\nPaste code here: ")
        self.assertEqual(events, [{"type": "authorization", "authorization_url": CLAUDE_URL},
                                  {"type": "input_required"}])

    def test_input_is_bounded_plain_authorization_code_only(self):
        self.assertTrue(guest_auth.valid_input("returned-code#state-part"))
        for value in ["short", "a" * 2049, "valid-code\nrm -rf /", "code\x1b[2J", "https://site/path", {}, "space code"]:
            self.assertFalse(guest_auth.valid_input(value))

    def test_disappearing_process_group_permission_race_keeps_cleanup_idempotent(self):
        process = Mock(pid=12345)
        process.poll.return_value = 0
        with patch("guest_auth.os.killpg", side_effect=PermissionError):
            guest_auth.stop_group(process)
        process.poll.return_value = None
        with patch("guest_auth.os.killpg", side_effect=PermissionError):
            with self.assertRaises(PermissionError):
                guest_auth.stop_group(process)

    def test_guest_transport_uses_explicit_user_home_isolated_python_and_no_shell(self):
        guest = GuestConfig(2000, 2001, "builder", "/home/builder", "/work", ":99")
        machine = SimpleNamespace(box=lambda: EngineBox("local", project="ebox-test", guest=guest))
        command = guest_command(machine, "ebox-private", "login", "claude")
        self.assertIn("--user=2000", command)
        self.assertIn("--group=2001", command)
        self.assertIn("--env=HOME=/home/builder", command)
        self.assertIn("--env=PATH=/home/builder/.local/bin:/usr/local/bin:/usr/bin:/bin", command)
        index = command.index("python3")
        self.assertEqual(command[index:index+4], ["python3", "-I", "-u", "-c"])
        self.assertNotIn("bash", command)


class ManagerTests(unittest.TestCase):
    def setUp(self):
        self.fleet = Mock()
        self.manager = AccountManager(self.fleet)
        self.addCleanup(self.manager.close)

    def test_guard_failure_removes_placeholder_and_no_process_starts(self):
        self.fleet.account_workspace.side_effect = ValueError("base forbidden")
        with patch("accounts.subprocess.Popen") as launch:
            with self.assertRaisesRegex(ValueError, "base forbidden"):
                self.manager.start("host", "ebox-base", "codex")
            launch.assert_not_called()
        self.assertFalse(self.manager.active("host", "ebox-base"))

    def test_active_registration_precedes_guard_and_duplicate_start_reuses_session(self):
        def guard(host, name):
            self.assertTrue(self.manager.active(host, name))
            return object()
        self.fleet.account_workspace.side_effect = guard
        with patch.object(self.manager, "_run", side_effect=lambda host, value: value.stop.wait(2)):
            first = self.manager.start("host", "ebox-work", "codex")
            second = self.manager.start("host", "ebox-work", "codex")
            self.assertEqual(first["id"], second["id"])
        self.assertEqual(self.fleet.account_workspace.call_count, 1)
        with self.assertRaises(ValueError):
            self.manager.session("other-host", "ebox-work", "codex", first["id"])

    def test_sixteen_active_connections_are_a_hard_limit(self):
        with patch.object(self.manager, "_run", side_effect=lambda host, value: value.stop.wait(2)):
            for i in range(16):
                self.manager.start("host", f"ebox-work-{i}", "codex")
            with self.assertRaisesRegex(ValueError, "Too many"):
                self.manager.start("host", "ebox-overflow", "claude")

    def test_remote_protocol_never_passes_raw_errors_tokens_or_unverified_success(self):
        value = _Session({"id": "a" * 32, "host_id": "host", "name": "ebox-work", "provider": "claude",
                          "status": "starting", "input_required": False, "message": "starting", "expires_at": "future"})
        self.manager._sessions[value.public["id"]] = value
        self.manager._event(value, {"type": "authorization", "authorization_url": "https://evil.example/?access_token=secret"})
        self.assertNotIn("authorization_url", value.public)
        self.manager._event(value, {"type": "result", "reason": "completed", "output": "private-secret",
                                   "account": {"id": "claude", "installed": True, "status": "disconnected", "auth_method": "none"}})
        self.assertEqual(value.public["status"], "failed")
        self.assertNotIn("private-secret", json.dumps(value.public))

    def test_status_uses_native_allowlist_and_drops_remote_extra_fields(self):
        native = {"type": "statuses", "providers": [
            {"id": "codex", "installed": True, "status": "connected", "auth_method": "api_key", "token": "secret"},
            {"id": "claude", "installed": True, "status": "unknown", "auth_method": "unknown", "detail": "secret"},
            {"id": "cursor", "installed": False, "status": "unavailable", "auth_method": "none"}]}
        with patch("accounts.guest_command", return_value=["unused"]), \
                patch("guest_auth.capture", return_value=(0, json.dumps(native).encode())):
            result = self.manager.status("host", "ebox-work")
        self.assertEqual(result["providers"][0]["auth_method"], "api_key")
        self.assertNotIn("secret", json.dumps(result))


FAKE_CLIENT = r'''
import json,os,pathlib,subprocess,sys,time
home=pathlib.Path(os.environ['HOME'])
provider=pathlib.Path(sys.argv[0]).name
if 'status' in sys.argv:
    if (home/'hang-status').exists():
        (home/'status-pid').write_text(str(os.getpid()))
        time.sleep(600)
    connected=(home/'connected').exists()
    if provider=='claude': print(json.dumps({'loggedIn':connected,'authMethod':'claude.ai' if connected else 'none','email':'private@example.com'}))
    elif provider=='agent': print(json.dumps({'isAuthenticated':connected,'message':'private-message'}))
    else: print('Logged in using ChatGPT' if connected else 'Not logged in')
    raise SystemExit(0 if connected or provider=='agent' else 1)
if provider=='claude':
    assert 'ANTHROPIC_API_KEY' not in os.environ
    print('https://claude.com/cai/oauth/authorize?response_type=code&state=test-state',flush=True)
    print('Paste code here: ',end='',flush=True)
    code=input()
    print('Do not expose submitted code: '+code,flush=True)
    (home/'connected').touch()
    raise SystemExit(0)
child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(600)'])
(home/'native-pids').write_text(str(os.getpid())+' '+str(child.pid))
print('https://auth.openai.com/codex/device',flush=True)
print('Enter this one-time code: ABCD-EFGH',flush=True)
while True: time.sleep(.2)
'''


class SupervisorIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        bindir = self.root / "bin"
        bindir.mkdir()
        for name in ["codex", "claude", "agent"]:
            path = bindir / name
            path.write_text("#!" + sys.executable + "\n" + FAKE_CLIENT)
            path.chmod(0o700)
        self.environment = patch.dict(os.environ, {"HOME": str(self.root), "PATH": str(bindir) + ":/usr/bin:/bin",
                                                   "ANTHROPIC_API_KEY": "never-emit-this-key"})
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.manager = AccountManager(Mock())
        self.addCleanup(self.manager.close)
        self.command = patch("accounts.guest_command", side_effect=lambda host, name, mode, provider:
                             [sys.executable, "-I", "-u", guest_auth.__file__, mode, provider])
        self.command.start()
        self.addCleanup(self.command.stop)

    def await_session(self, session, predicate, timeout=6):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            value = self.manager.session(session["host_id"], session["name"], session["provider"], session["id"])
            if predicate(value):
                return value
            time.sleep(.02)
        self.fail("Native auth session did not reach expected state: " + str(value))

    def test_claude_code_is_not_echoed_or_retained_and_status_confirms_success(self):
        session = self.manager.start("host", "ebox-work", "claude")
        waiting = self.await_session(session, lambda item: item["input_required"])
        self.assertEqual(waiting["status"], "waiting")
        code = "private-authorization-code#private-state"
        self.manager.input("host", "ebox-work", "claude", session["id"], code)
        final = self.await_session(session, lambda item: item["status"] in accounts.TERMINAL)
        self.assertEqual(final["status"], "succeeded", final)
        self.assertNotIn(code, json.dumps(final))
        self.assertNotIn("authorization_url", final)
        self.assertFalse(final["input_required"])
        self.assertFalse(self.manager.active("host", "ebox-work"))

    def test_cancel_kills_native_process_and_descendant_and_releases_provider_lock(self):
        session = self.manager.start("host", "ebox-work", "codex")
        self.await_session(session, lambda item: "user_code" in item)
        parent, child = map(int, (self.root / "native-pids").read_text().split())
        result = self.manager.cancel("host", "ebox-work", "codex", session["id"])
        self.assertEqual(result["status"], "cancelled")
        self.assertNotIn("user_code", result)
        for pid in [parent, child]:
            result = subprocess.run(["/bin/ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
            self.assertTrue(result.returncode != 0 or result.stdout.strip().startswith("Z"), f"Native process {pid} survived cancellation")
        again = self.manager.start("host", "ebox-work", "codex")
        self.await_session(again, lambda item: "user_code" in item)

    def test_cancel_interrupts_post_login_native_status_verification(self):
        (self.root / "hang-status").touch()
        session = self.manager.start("host", "ebox-work", "claude")
        self.await_session(session, lambda item: item["input_required"])
        self.manager.input("host", "ebox-work", "claude", session["id"], "returned-code#returned-state")
        deadline = time.monotonic() + 5
        while not (self.root / "status-pid").exists() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertTrue((self.root / "status-pid").exists(), "Native verification did not start")
        pid = int((self.root / "status-pid").read_text())
        final = self.manager.cancel("host", "ebox-work", "claude", session["id"])
        self.assertEqual(final["status"], "cancelled")
        result = subprocess.run(["/bin/ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
        self.assertTrue(result.returncode != 0 or result.stdout.strip().startswith("Z"))

    def test_two_controllers_cannot_start_same_provider_and_eof_cancels(self):
        session = self.manager.start("host", "ebox-work", "codex")
        self.await_session(session, lambda item: "user_code" in item)
        other = AccountManager(Mock())
        try:
            duplicate = other.start("host", "ebox-work", "codex")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                result = other.session("host", "ebox-work", "codex", duplicate["id"])
                if result["status"] in accounts.TERMINAL:
                    break
                time.sleep(.02)
            self.assertEqual(result["status"], "failed")
            self.assertIn("already running", result["message"])
        finally:
            other.close()
        value = self.manager._sessions[session["id"]]
        value.process.stdin.close()  # lost controller transport
        final = self.await_session(session, lambda item: item["status"] in accounts.TERMINAL)
        self.assertEqual(final["status"], "cancelled")

    def test_guest_deadline_expires_without_controller_cooperation(self):
        source = Path(guest_auth.__file__).read_text().replace('if __name__ == "__main__":', 'if False:')
        process = subprocess.Popen([sys.executable, "-I", "-u", "-c", source + '\nlogin("codex", seconds=1)'],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            # Keep stdin open: expiry must be the guest's own deadline.
            process.wait(timeout=5)
            output = process.stdout.read()
            events = [json.loads(line) for line in output.splitlines()]
            self.assertEqual(events[-1]["reason"], "expired")
        finally:
            process.kill() if process.poll() is None else None
            process.communicate(timeout=2)


if __name__ == "__main__":
    unittest.main()
