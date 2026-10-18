"""Offline regression checks. Never connects to SSH or Incus."""
import asyncio
from contextlib import redirect_stderr
import importlib.util
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import unittest
from unittest.mock import patch

import enginebox
from enginebox import EngineBox, GuestConfig


class OfflineTestCase(unittest.TestCase):
    def setUp(self):
        # Each test starts without personal host settings. Every subprocess is
        # denied unless a test explicitly supplies a fake result.
        self.environment = patch.dict(os.environ, {}, clear=True)
        self.environment.start()
        self.addCleanup(self.environment.stop)
        self.process = patch("enginebox.subprocess.run", side_effect=AssertionError("Unexpected subprocess"))
        self.run = self.process.start()
        self.addCleanup(self.process.stop)


class RuntimeTests(OfflineTestCase):
    def test_operator_requires_explicit_host(self):
        with self.assertRaisesRegex(ValueError, "Set ENGINEBOX_HOST"):
            EngineBox()
        os.environ["ENGINEBOX_HOST"] = "server-alias"
        self.assertEqual(EngineBox().host, "server-alias")
        self.assertEqual(EngineBox("local").host, "local")

    def test_host_rejects_options_shell_syntax_and_ambiguous_usernames(self):
        for host in ["", "-oProxyCommand=bad", "host;bad", "$(bad)", "host name", "a@b@c", "host\n"]:
            with self.subTest(host=host), self.assertRaises(ValueError):
                EngineBox(host)
        for host in ["local", "builder@my-server", "10.1.2.3", "server.example", "my_alias"]:
            with self.subTest(host=host):
                self.assertEqual(EngineBox(host).host, host)

    def test_guest_defaults(self):
        guest = GuestConfig.from_env()
        self.assertEqual((guest.uid, guest.gid, guest.user, guest.home, guest.workspace, guest.display),
                         (1001, 1001, "agent", "/home/agent", "/workspace", ":99"))
        self.assertEqual(EngineBox("local").project, "enginebox")

    def test_guest_config_is_applied_as_literal_arguments(self):
        os.environ.update({"ENGINEBOX_GUEST_UID": "2001", "ENGINEBOX_GUEST_GID": "2002",
                           "ENGINEBOX_GUEST_USER": "builder", "ENGINEBOX_GUEST_HOME": "/home/my builder",
                           "ENGINEBOX_GUEST_WORKSPACE": "/work/literal $(marker)",
                           "ENGINEBOX_GUEST_DISPLAY": ":1.0"})
        args = EngineBox("local").exec_args("ebox-work", ["pwd"])
        for option in ["--user=2001", "--group=2002", "--env=USER=builder",
                       "--env=HOME=/home/my builder", "--cwd=/work/literal $(marker)", "--env=DISPLAY=:1.0"]:
            self.assertIn(option, args)

    def test_invalid_guest_config_fails_before_transport(self):
        invalid = {
            "ENGINEBOX_GUEST_UID": ["0", "-1", "abc", "4294967295", ""],
            "ENGINEBOX_GUEST_GID": ["0", "1;id"],
            "ENGINEBOX_GUEST_USER": ["root", "-agent", "agent;id", ""],
            "ENGINEBOX_GUEST_HOME": ["relative", "/home/../root", "/home/agent\r"],
            "ENGINEBOX_GUEST_WORKSPACE": ["relative", "/work\nspace"],
            "ENGINEBOX_GUEST_DISPLAY": ["remote:0", ":99;id", ""],
            "ENGINEBOX_PROJECT": ["", "-project", "a/b", "a?project=b", "a" * 64],
        }
        for variable, values in invalid.items():
            for value in values:
                with self.subTest(variable=variable, value=value), patch.dict(os.environ, {variable: value}):
                    with self.assertRaisesRegex(ValueError, variable):
                        EngineBox("local")
        self.run.assert_not_called()

    def test_unknown_guest_mode_is_not_treated_as_operator(self):
        with patch.dict(os.environ, {"ENGINEBOX_GUEST": "true"}):
            with self.assertRaisesRegex(ValueError, "ENGINEBOX_GUEST"):
                EngineBox("local")
            with self.assertRaises(ValueError):
                enginebox.computer(["true"])

    def test_remote_argv_survives_both_shell_boundaries(self):
        argv = ["printf", "$(touch /tmp/nope); `id`", "two words", "'quoted'", "", "line\nbreak"]
        box = EngineBox("builder@server")
        command = box.command(*box.exec_args("ebox-work", argv))
        self.assertEqual(command[0], "ssh")
        self.assertEqual(command[-2], "builder@server")
        incus_argv = shlex.split(command[-1])
        self.assertEqual(incus_argv[:5], ["sudo", "-n", "incus", "--project", "enginebox"])
        self.assertEqual(incus_argv[-3:-1], ["bash", "-lc"])
        self.assertEqual(shlex.split(incus_argv[-1]), ["exec", "--", *argv])
        self.run.assert_not_called()

    def test_local_transport_uses_no_shell_or_ssh(self):
        self.assertEqual(EngineBox("local").command("list", "--format=json"),
                         ["sudo", "-n", "incus", "--project", "enginebox", "list", "--format=json"])

    def test_project_applies_to_commands_and_raw_metadata_query(self):
        os.environ["ENGINEBOX_PROJECT"] = "team-workspaces"
        box = EngineBox("local")
        response = {"name": "ebox-work", "status": "Stopped"}
        self.run.side_effect = None
        self.run.return_value = subprocess.CompletedProcess([], 0, json.dumps(response).encode(), b"")
        self.assertEqual(box.status("ebox-work")["status"], "Stopped")
        self.assertEqual(self.run.call_args.args[0],
                         ["sudo", "-n", "incus", "query", "/1.0/instances/ebox-work?project=team-workspaces"])
        self.assertIn("team-workspaces", box.command("list"))

    def test_invalid_instance_or_argv_never_reaches_transport(self):
        box = EngineBox("local")
        for name in ["other-vm", "ebox-dev;id", "ebox-dev/one", "-ebox-dev"]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                box.status(name)
        for argv in [[], ["printf", "bad\0value"]]:
            with self.subTest(argv=argv), self.assertRaises(ValueError):
                box.execute("ebox-work", argv)
        for timeout in [0, 301]:
            with self.subTest(timeout=timeout), self.assertRaises(ValueError):
                box.execute("ebox-work", ["true"], timeout)
        self.run.assert_not_called()

    def test_destructive_lifecycle_rejects_invalid_states(self):
        box = EngineBox("local")
        invalid = [("Frozen", "stop"), ("Stopped", "hibernate"), ("Stopped", "pause"),
                   ("Running", "resume"), ("Running", "delete")]
        for state, action in invalid:
            with self.subTest(state=state, action=action), patch.object(box, "status", return_value={"status": state}):
                with self.assertRaises(ValueError):
                    box.lifecycle("ebox-work", action)
        self.run.assert_not_called()

    def test_restore_rejects_running_and_frozen_instances(self):
        box = EngineBox("local")
        for state in ["Running", "Frozen"]:
            with self.subTest(state=state), patch.object(box, "status", return_value={"status": state}):
                with self.assertRaisesRegex(ValueError, "Stop the VM explicitly"):
                    box.restore("ebox-work", "saved")
        self.run.assert_not_called()

    def test_snapshot_rejects_frozen_or_hibernated_instance(self):
        box = EngineBox("local")
        for state in [{"status": "Frozen"}, {"status": "Stopped", "stateful": True}]:
            with self.subTest(state=state), patch.object(box, "status", return_value=state):
                with self.assertRaises(ValueError):
                    box.snapshot("ebox-work", "saved")
        self.run.assert_not_called()

    def test_restore_requires_valid_snapshot_and_preserves_disk_only_choice(self):
        box = EngineBox("local")
        with patch.object(box, "status", return_value={"status": "Stopped"}), patch.object(box, "run") as run:
            with self.assertRaises(ValueError):
                box.restore("ebox-work", "saved;id")
            run.assert_not_called()
            box.restore("ebox-work", "saved", stateful=False)
            run.assert_called_once_with("snapshot", "restore", "ebox-work", "saved")

    def test_desktop_handles_local_and_remote_hosts(self):
        inventory = [{"name": "ebox-work", "ipv4": ["10.1.2.3"]}]
        for host in ["local", "builder@server"]:
            with self.subTest(host=host), patch.object(EngineBox, "list", return_value=inventory):
                desktop = EngineBox(host).desktop("ebox-work")
                if host == "local":
                    self.assertIsNone(desktop["tunnel"])
                    self.assertIsNone(desktop["tunnel_argv"])
                    self.assertEqual(desktop["url"], "http://10.1.2.3:6080/vnc.html?autoconnect=true")
                else:
                    self.assertEqual(shlex.split(desktop["tunnel"]), desktop["tunnel_argv"])
                    self.assertEqual(desktop["tunnel_argv"][-1], host)
                    self.assertIn("127.0.0.1:16080:10.1.2.3:6080", desktop["tunnel_argv"])

    def test_desktop_rejects_untrusted_address_from_inventory(self):
        with patch.object(EngineBox, "list", return_value=[{"name": "ebox-work", "ipv4": ["1.2.3.4;id"]}]):
            with self.assertRaises(ValueError):
                EngineBox("server").desktop("ebox-work")

    def test_missing_host_is_a_clean_cli_error(self):
        stderr = io.StringIO()
        with patch.object(sys, "argv", ["ebox", "list"]), redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as exit_status:
                enginebox.main()
        self.assertEqual(exit_status.exception.code, 1)
        self.assertIn("Set ENGINEBOX_HOST explicitly", stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())


class MCPModeTests(OfflineTestCase):
    # Separate module instances ensure changing the environment cannot leave
    # tools registered from an earlier test's operator mode.
    def load_mcp(self):
        path = Path(enginebox.__file__).with_name("mcp_server.py")
        spec = importlib.util.spec_from_file_location("enginebox_mcp_test", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def test_guest_mcp_exposes_only_desktop_tools_and_needs_no_host(self):
        os.environ.update({"ENGINEBOX_GUEST": "1", "ENGINEBOX_GUEST_DISPLAY": ":12"})
        with patch("enginebox.EngineBox", side_effect=AssertionError("Guest attempted host access")):
            module = self.load_mcp()
            tools = asyncio.run(module.mcp.list_tools())
            self.assertEqual({tool.name for tool in tools},
                             {"desktop_screenshot", "desktop_click", "desktop_type", "desktop_key"})
            self.run.side_effect = None
            self.run.return_value = subprocess.CompletedProcess([], 0, b"", b"")
            module.desktop_type("literal $(value)")
            self.assertEqual(self.run.call_args.args[0],
                             ["xdotool", "type", "--clearmodifiers", "--", "literal $(value)"])
            self.assertEqual(self.run.call_args.kwargs["env"]["DISPLAY"], ":12")

    def test_operator_mcp_retains_ten_tools(self):
        os.environ["ENGINEBOX_HOST"] = "test-server"
        module = self.load_mcp()
        names = {tool.name for tool in asyncio.run(module.mcp.list_tools())}
        self.assertEqual(len(names), 10)
        self.assertIn("environment_restore", names)
        self.run.assert_not_called()

    def test_guest_mode_cannot_construct_operator(self):
        os.environ["ENGINEBOX_GUEST"] = "1"
        with self.assertRaisesRegex(ValueError, "unavailable"):
            EngineBox("local")
        self.run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
