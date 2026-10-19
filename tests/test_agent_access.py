"""Controller credential lifecycle tests use only synthetic native credentials."""
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from enginebox import GuestConfig
import agent_access as access


UUID = "11111111-2222-3333-4444-555555555555"
REFRESH = "private-rotating-refresh-never-delegate"
SETUP = "sk-ant-oat01-" + "privateSetupToken" * 5


def jwt(expiry=None):
    data = base64.urlsafe_b64encode(json.dumps({"exp": expiry or time.time() + 3600}).encode()).decode().rstrip("=")
    return "header." + data + ".private-signature"


def native(provider, expiry=None):
    if provider == "codex":
        return {"auth_mode": "chatgpt", "OPENAI_API_KEY": None, "last_refresh": "2026-10-19T00:00:00Z",
                "tokens": {"id_token": "private-id-token", "access_token": jwt(expiry), "refresh_token": REFRESH, "account_id": "private-account"}}
    if provider == "claude":
        return {"claudeAiOauth": {"accessToken": "private-claude-access", "refreshToken": REFRESH,
                "expiresAt": (expiry or time.time() + 3600) * 1000, "scopes": ["user:profile", "user:inference"]}}
    return {"accessToken": jwt(expiry), "refreshToken": REFRESH}


def guest_script(root):
    """Exercise the fixed guest program with synthetic trusted identity files."""
    for name in ["identity", "machine-id"]:
        (root / name).write_text(UUID.replace("-", "") + "\n")
        (root / name).chmod(0o644)
    return (access.GUEST_TRANSFER.replace('IDENTITY_OWNER = 0', 'IDENTITY_OWNER = ' + str(os.getuid()))
            .replace('("/var/lib/enginebox/instance-uuid", "/etc/machine-id")', repr((str(root / "identity"), str(root / "machine-id")))))


class AgentAccessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.fleet = Mock()
        self.host = SimpleNamespace(target="server", project="ebox", pool="pool", guest=GuestConfig(1001, 1001, "agent", "/home/agent", "/workspace", ":99"))
        self.fleet.account_workspace.return_value = self.host
        self.agents = access.AgentAccess(self.fleet, self.root / "agents", binaries={p: "/nonexistent/" + p for p in access.PROVIDERS})
        self.addCleanup(self.agents.close)

    def save(self, provider, value=None):
        home = self.agents.home(provider)
        access.write_private(home / access.NATIVE_FILES[provider], value or native(provider), home)

    def test_delegation_never_contains_refresh_credentials(self):
        for provider in access.PROVIDERS:
            value = access.native_material(provider, native(provider))
            self.assertNotIn(REFRESH, json.dumps(value))
            self.assertIsNotNone(value["expires_at"])
        self.assertEqual(access.native_material("codex", native("codex"))["content"]["auth_mode"], "chatgptAuthTokens")
        self.assertEqual(set(access.native_material("claude", native("claude"))["content"]), {"CLAUDE_CODE_OAUTH_TOKEN"})

    def test_private_store_rejects_symlinks_and_uses_private_modes(self):
        self.save("codex")
        home = self.agents.home("codex")
        self.assertEqual(home.stat().st_mode & 0o777, 0o700)
        self.assertEqual((home / access.NATIVE_FILES["codex"]).stat().st_mode & 0o777, 0o600)
        other = self.agents.home("claude")
        (other / ".claude").symlink_to(home / ".codex", target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symbolic"):
            self.save("claude")
        insecure = self.root / "insecure"
        insecure.mkdir(mode=0o755)
        with self.assertRaisesRegex(ValueError, "private"):
            access.AgentAccess(self.fleet, insecure)

    def test_base_and_unassigned_workspace_guards_precede_credential_reads(self):
        self.fleet.account_workspace.side_effect = ValueError("not assigned")
        with patch("agent_access.transfer") as transfer:
            for operation in [lambda: self.agents.adopt("host", "ebox-base", "codex"), lambda: self.agents.attach("host", "ebox-base")]:
                with self.assertRaisesRegex(ValueError, "not assigned"):
                    operation()
            transfer.assert_not_called()

    def test_adoption_moves_refresh_ownership_and_registers_source(self):
        calls = []
        def transfer(host, name, request):
            calls.append(request)
            return {"ok": True, "native": native(request["provider"]), "instance_id": UUID}
        with patch("agent_access.transfer", side_effect=transfer):
            result = self.agents.adopt("host", "ebox-source", "claude")
        self.assertEqual(result["status"], "adopted")
        self.assertEqual([r["operation"] for r in calls], ["export", "attach"])
        self.assertTrue(calls[1]["retire_refresh"])
        self.assertNotIn(REFRESH, json.dumps(calls[1]))
        self.assertEqual(calls[1]["expected_instance_id"], UUID)
        home = self.agents.home("claude")
        self.assertEqual(access.read_private(home / access.NATIVE_FILES["claude"], home)["claudeAiOauth"]["refreshToken"], REFRESH)
        self.assertNotIn(REFRESH, json.dumps(result))
        self.assertEqual(len(list(self.agents.receipts.glob("*.json"))), 1)

    def test_interrupted_adoption_retains_refresh_but_cannot_delegate_until_finished(self):
        with patch("agent_access.transfer", side_effect=[{"ok": True, "native": native("codex"), "instance_id": UUID}, ValueError("disconnected")]):
            with self.assertRaises(ValueError):
                self.agents.adopt("host", "ebox-source", "codex")
        with self.assertRaisesRegex(ValueError, "pending"):
            self.agents._material("codex")
        with patch("agent_access.transfer", return_value={"ok": True, "instance_id": UUID}) as transfer:
            self.agents.adopt("host", "ebox-source", "codex")
        self.assertEqual(transfer.call_count, 1, "retry must retain the staged refresh credential instead of rereading downgraded source")
        self.assertFalse((self.agents.home("codex") / "pending-adoption.json").exists())

    def test_attach_records_private_binding_and_reconcile_passes_expected_uuid(self):
        self.save("codex")
        with patch("agent_access.transfer", return_value={"ok": True, "instance_id": UUID}) as transfer:
            result = self.agents.attach("host", "ebox-work", ["codex"])
        request = transfer.call_args.args[2]
        self.assertNotIn(REFRESH, json.dumps(request))
        self.assertEqual(request["expected_version"], access.CODEX_VERSION)
        self.assertEqual(result["providers"][0]["status"], "attached")
        callback = Mock(return_value={"providers": []})
        self.agents.reconcile(callback)
        self.assertEqual(callback.call_args.kwargs["expected_instance_id"], UUID)
        with patch("agent_access.transfer", return_value={"ok": True, "instance_id": UUID}) as transfer:
            self.agents.attach("host", "ebox-work", ["codex"])
        self.assertEqual(transfer.call_args.args[2]["expected_instance_id"], UUID)
        with self.assertRaisesRegex(ValueError, "host has changed"):
            self.agents.attach("host", "ebox-work", ["codex"], expected_host_binding="stale")
        self.host.target = "rebound-host"
        with self.assertRaisesRegex(ValueError, "host has changed"):
            self.agents.attach("host", "ebox-work", ["codex"])

    def test_expired_tokens_are_not_sent_when_native_refresh_fails(self):
        self.save("codex", native("codex", time.time() - 10))
        with patch.object(self.agents, "_refresh", side_effect=ValueError("private provider error")) as refresh, patch("agent_access.transfer") as transfer:
            result = self.agents.attach("host", "ebox-work", ["codex"])
            self.agents.attach("host", "ebox-work", ["codex"])
        transfer.assert_not_called()
        self.assertEqual(refresh.call_count, 1, "a possibly consumed refresh token must not be retried for each workspace")
        self.assertEqual(result["providers"][0]["status"], "reauth_required")
        self.assertNotIn("private provider error", json.dumps(result))

    def test_environment_contains_no_caller_credentials_or_config(self):
        with patch.dict(os.environ, {"OPENAI_API_KEY": "private-secret", "ANTHROPIC_BASE_URL": "https://wrong", "PYTHONPATH": "/repo"}):
            env = access.controller_environment(self.root, {"codex": "/safe/bin/codex"})
        for key in ["OPENAI_API_KEY", "ANTHROPIC_BASE_URL", "PYTHONPATH"]:
            self.assertNotIn(key, env)
        command = self.agents._command(self.agents.home("codex"), "codex", "login", "codex")
        self.assertEqual(command[:4], [sys.executable, "-I", "-u", "-c"])
        self.assertNotIn("private-secret", json.dumps(command))

    def test_only_cursor_key_input_is_supported_and_never_returned(self):
        result = self.agents.connect_key("cursor", "private-native-cursor-key")
        self.assertNotIn("private-native-cursor-key", json.dumps(result))
        self.assertNotIn("refreshToken", json.dumps(self.agents._material("cursor")))
        for provider, token in [("codex", "private-key"), ("cursor", "key\ncommand"), ("cursor", "x" * 17000)]:
            with self.assertRaises(ValueError):
                self.agents.connect_key(provider, token)

    def test_starting_login_blocks_adoption_before_guest_lock_is_acquired(self):
        with patch.object(self.agents.manager, "active", return_value=True), patch("agent_access.transfer") as transfer:
            with self.assertRaisesRegex(ValueError, "busy"):
                self.agents.adopt("host", "ebox-source", "codex")
            transfer.assert_not_called()

    def test_native_claude_refresh_passes_secret_only_in_environment(self):
        self.save("claude", native("claude", time.time() - 1))
        with patch("guest_auth.capture", return_value=(0, b"")) as capture:
            self.agents._refresh("claude", self.agents.home("claude"))
        command, environment = capture.call_args.args
        self.assertEqual(command[1:], ["auth", "login", "--claudeai"])
        self.assertNotIn(REFRESH, json.dumps(command))
        self.assertEqual(environment["CLAUDE_CODE_OAUTH_REFRESH_TOKEN"], REFRESH)
        self.assertEqual(environment["CLAUDE_CODE_OAUTH_SCOPES"], "user:profile user:inference")

    def test_provider_lock_is_shared_across_controller_objects(self):
        other = access.AgentAccess(self.fleet, self.root / "agents")
        try:
            with self.agents._provider_lock("codex"):
                with self.assertRaisesRegex(ValueError, "busy"):
                    with other._provider_lock("codex"):
                        self.fail("A second controller acquired the native provider lock")
        finally:
            other.close()


FAKE_CLAUDE = r'''
import json,os,pathlib,sys
if "status" in sys.argv:
    logged = os.environ.get("CLAUDE_CODE_OAUTH_TOKEN", "").startswith("sk-ant-oat01-")
    print(json.dumps({"loggedIn":logged,"authMethod":"oauth_token" if logged else "none"}))
    raise SystemExit(0 if logged else 1)
assert sys.argv[1:] == ["setup-token"]
assert "ANTHROPIC_API_KEY" not in os.environ
print("https://claude.com/cai/oauth/authorize?code=true&state=fake\nPaste code here: ",flush=True)
input()
print("Your token: sk-ant-oat01-" + "privateSetupToken" * 5,flush=True)
'''


class ControllerFlowTests(unittest.TestCase):
    def test_first_codex_status_creates_custom_home_before_native_client_runs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "codex"
            binary.write_text("#!" + sys.executable + "\nimport os,pathlib,sys\nassert pathlib.Path(os.environ['CODEX_HOME']).is_dir()\nprint('Not logged in')\nsys.exit(1)\n")
            binary.chmod(0o700)
            agents = access.AgentAccess(Mock(), root / "agents", binaries={"codex": str(binary)})
            try:
                self.assertFalse((agents.home("codex") / ".codex").exists())
                result = agents.manager.status("controller", "codex")
                codex = next(p for p in result["providers"] if p["id"] == "codex")
                self.assertEqual(codex["status"], "disconnected")
                self.assertEqual((agents.home("codex") / ".codex").stat().st_mode & 0o777, 0o700)
            finally:
                agents.close()

    def test_codex_refresh_uses_official_app_server_protocol_without_credentials_in_argv(self):
        script = r'''
import json,os,pathlib,sys
home=pathlib.Path(os.environ["HOME"])
seen=[]
for line in sys.stdin:
    value=json.loads(line); seen.append(value)
    if value.get("id")==1: print(json.dumps({"id":1,"result":{}}),flush=True)
    if value.get("id")==2:
        assert value["method"]=="account/read" and value["params"]=={"refreshToken":True}
        (home/"rpc.json").write_text(json.dumps(seen))
        print(json.dumps({"id":2,"result":{"account":{"type":"chatgpt"}}}),flush=True)
'''
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "codex"
            binary.write_text("#!" + sys.executable + "\n" + script)
            binary.chmod(0o700)
            access._refresh_codex(str(binary), {"HOME": str(root), "PATH": "/usr/bin:/bin"})
            calls = json.loads((root / "rpc.json").read_text())
            self.assertEqual([c["method"] for c in calls], ["initialize", "initialized", "account/read"])
            self.assertNotIn(REFRESH, json.dumps(calls))

    def test_setup_token_flow_persists_token_privately_without_exposing_it(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "claude"
            binary.write_text("#!" + sys.executable + "\n" + FAKE_CLAUDE)
            binary.chmod(0o700)
            agents = access.AgentAccess(Mock(), root / "agents", binaries={"claude": str(binary)})
            try:
                session = agents.start("claude")
                deadline = time.monotonic() + 8
                while not session["input_required"] and time.monotonic() < deadline:
                    time.sleep(.02)
                    session = agents.session("claude", session["id"])
                self.assertTrue(session["input_required"], session)
                self.assertEqual(session["scope"], "controller")
                self.assertNotIn("host_id", session)
                agents.input("claude", session["id"], "private-browser-returned-code")
                while session["status"] in {"starting", "waiting"} and time.monotonic() < deadline:
                    time.sleep(.02)
                    session = agents.session("claude", session["id"])
                self.assertEqual(session["status"], "succeeded", session)
                self.assertNotIn(SETUP, json.dumps(session))
                self.assertNotIn("private-browser-returned-code", json.dumps(session))
                material = agents._material("claude")
                self.assertEqual(material["content"]["CLAUDE_CODE_OAUTH_TOKEN"], SETUP)
                path = agents.home("claude") / access.TOKEN_FILE
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            finally:
                agents.close()

    def test_setup_token_cancel_does_not_save_a_credential(self):
        token = access._SetupToken(Path("/unused"))
        token.output("Your token: " + SETUP + "\n")
        with patch("agent_access.write_private") as write:
            result = token.complete({}, lambda: True)
        write.assert_not_called()
        self.assertEqual(result["status"], "unknown")


class GuestDeliveryTests(unittest.TestCase):
    def test_delivery_preserves_claude_settings_retires_refresh_and_is_idempotent(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "identity").write_text(UUID)
            (root / ".claude").mkdir()
            (root / ".claude/settings.json").write_text(json.dumps({"theme": "dark", "env": {"OTHER": "keep"}}))
            (root / ".claude/.credentials.json").write_text(json.dumps(native("claude")))
            script = guest_script(root)
            request = access.AgentAccess._attachment("claude", access.native_material("claude", native("claude")), retire_refresh=True)
            def run(value):
                result = subprocess.run([sys.executable, "-I", "-c", script], input=json.dumps(value).encode(), capture_output=True, env={**os.environ, "HOME": str(root)})
                return result.returncode, json.loads(result.stdout)
            self.assertEqual(run({**request, "retire_refresh": False})[0], 1,
                             "initial discovery must preserve an existing workspace-native login")
            self.assertTrue((root / ".claude/.credentials.json").exists())
            code, result = run(request)
            self.assertEqual(code, 0, result)
            settings = json.loads((root / ".claude/settings.json").read_text())
            self.assertEqual(settings["env"]["OTHER"], "keep")
            self.assertEqual(settings["theme"], "dark")
            self.assertFalse((root / ".claude/.credentials.json").exists())
            self.assertEqual((root / ".claude/settings.json").stat().st_mode & 0o777, 0o600)
            self.assertTrue(run(request)[1]["unchanged"])
            self.assertEqual(run({**request, "expected_instance_id": "different"})[0], 1)
            (root / ".claude/.credentials.json").write_text(json.dumps(native("claude")))
            self.assertEqual(run({**request, "retire_refresh": False})[0], 1, "a new native login must take priority over controller reconciliation")
            (root / ".claude/.credentials.json").unlink()
            settings["env"]["CLAUDE_CODE_OAUTH_TOKEN"] = "manually-connected-workspace-token"
            (root / ".claude/settings.json").write_text(json.dumps(settings))
            self.assertEqual(run({**request, "generation": "f" * 64, "retire_refresh": False})[0], 1)
            self.assertEqual(json.loads((root / ".claude/settings.json").read_text())["env"]["CLAUDE_CODE_OAUTH_TOKEN"], "manually-connected-workspace-token")

    def test_delivery_rejects_symlink_credential_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "identity").write_text(UUID)
            (root / "elsewhere").mkdir()
            (root / ".claude").symlink_to(root / "elsewhere", target_is_directory=True)
            script = guest_script(root)
            request = access.AgentAccess._attachment("claude", access.native_material("claude", native("claude")))
            result = subprocess.run([sys.executable, "-I", "-c", script], input=json.dumps(request).encode(), capture_output=True, env={**os.environ, "HOME": str(root)})
            self.assertEqual(result.returncode, 1)
            self.assertEqual(json.loads(result.stdout), {"ok": False})
            self.assertEqual(list((root / "elsewhere").iterdir()), [])

    def test_identity_uses_readable_owned_marker_and_rejects_mismatch_or_writable_files(self):
        self.assertNotIn("/sys/class/dmi/id/product_uuid", access.GUEST_TRANSFER)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = guest_script(root)
            (root / ".codex").mkdir()
            (root / ".codex/auth.json").write_text(json.dumps(native("codex")))
            def run(source=script):
                result = subprocess.run([sys.executable, "-I", "-c", source], input=b'{"operation":"export","provider":"codex"}',
                                        capture_output=True, env={**os.environ, "HOME": str(root)})
                return result.returncode, json.loads(result.stdout)
            code, value = run()
            self.assertEqual(code, 0)
            self.assertEqual(value["instance_id"], UUID.replace("-", ""))
            (root / "machine-id").write_text("f" * 32)
            self.assertEqual(run(), (1, {"ok": False}))
            (root / "machine-id").write_text(UUID.replace("-", ""))
            (root / "identity").chmod(0o666)
            self.assertEqual(run(), (1, {"ok": False}))
            (root / "identity").chmod(0o644)
            self.assertEqual(run(script.replace('IDENTITY_OWNER = ' + str(os.getuid()), 'IDENTITY_OWNER = 123456789')), (1, {"ok": False}))
            (root / "identity").unlink()
            (root / "identity").symlink_to(root / "machine-id")
            self.assertEqual(run(), (1, {"ok": False}))


if __name__ == "__main__":
    unittest.main()
