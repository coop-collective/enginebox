"""Offline installer validation tests; no systemd, network or host mutations."""
import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import types
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("control_install", Path(__file__).with_name("install.py"))
install = importlib.util.module_from_spec(spec)
spec.loader.exec_module(install)


class InstallerTests(unittest.TestCase):
    def values(self):
        return {"ENGINEBOX_API_TOKEN": "a" * 40, "ENGINEBOX_DASHBOARD_PASSWORD": "b" * 24,
                "ENGINEBOX_SESSION_SECRET": "c" * 40}

    def test_missing_venv_seed_has_actionable_guidance_without_mutation(self):
        with patch.object(install.subprocess, "run", return_value=types.SimpleNamespace(returncode=1)) as run:
            with self.assertRaisesRegex(ValueError, "Install python3-venv"):
                install.check_venv_seed()
        command = run.call_args.args[0]
        self.assertEqual(command[:4], [install.sys.executable, "-I", "-B", "-c"])
        self.assertNotIn("bootstrap(", command[4])

    def test_available_venv_seed_passes_preflight_probe(self):
        with patch.object(install.subprocess, "run", return_value=types.SimpleNamespace(returncode=0)):
            install.check_venv_seed()

    def test_input_permissions_and_symlinks_are_checked(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "private.env"
            path.write_text("not a real secret")
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                install.private_file(path)
            path.chmod(0o600)
            install.private_file(path)
            link = Path(directory) / "link.env"
            link.symlink_to(path)
            with self.assertRaises(ValueError):
                install.private_file(link)

    def test_environment_is_parsed_as_data_not_shell_code(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "private.env"
            path.write_text("ENGINEBOX_API_TOKEN='$(touch /never-execute)'\n")
            self.assertEqual(install.read_environment(path)["ENGINEBOX_API_TOKEN"], "$(touch /never-execute)")
            for text in ["LD_PRELOAD=evil\n", "PORT=3000\nPORT=4000\n", "export PORT=3000\n"]:
                path.write_text(text)
                with self.assertRaises(ValueError):
                    install.read_environment(path)

    def test_loopback_defaults_and_explicit_origin(self):
        values = install.validate_environment(self.values(), 8765)
        self.assertEqual(values["HOST"], "127.0.0.1")
        self.assertEqual(values["PORT"], "3000")
        self.assertEqual(values["ENGINEBOX_API_URL"], "http://127.0.0.1:8765")
        secure = install.validate_environment({**self.values(), "ENGINEBOX_DASHBOARD_ORIGIN": "https://fleet.example"}, 8765)
        self.assertEqual(secure["ENGINEBOX_DASHBOARD_ORIGIN"], "https://fleet.example")

    def test_unsafe_credentials_bindings_and_ports_are_rejected(self):
        for overrides in [{"ENGINEBOX_API_TOKEN": "short"}, {"HOST": "0.0.0.0"}, {"PORT": "8765"},
                          {"PORT": "80"}, {"ENGINEBOX_API_URL": "http://remote.example"},
                          {"ENGINEBOX_DASHBOARD_ORIGIN": "https://fleet.example/path"},
                          {"ENGINEBOX_DASHBOARD_PASSWORD": "a" * 40}]:
            with self.subTest(overrides=list(overrides)), self.assertRaises(ValueError):
                install.validate_environment({**self.values(), **overrides}, 8765)

    def test_systemd_paths_reject_specifiers_and_control_characters(self):
        for path in ["/opt/test%u", "/opt/../etc", "/opt/a\nb", "/opt/space path"]:
            with self.subTest(path=path), self.assertRaises(ValueError):
                install.safe_path(Path(path))

    def test_default_plan_does_not_apply_or_print_credentials(self):
        user = types.SimpleNamespace(pw_name="operator")
        values = {**self.values(), "PORT": "3000"}
        output = io.StringIO()
        with patch("sys.argv", ["install.py", "--config", "/tmp/fleet.toml", "--env-file", "/tmp/private.env", "--user", "operator"]), \
                patch.object(install, "preflight", return_value=(user, "operator", "/usr/bin/node", values)), \
                patch.object(install, "apply") as apply, contextlib.redirect_stdout(output):
            install.main()
        apply.assert_not_called()
        self.assertIn("Plan only; no changes made", output.getvalue())
        for value in self.values().values():
            self.assertNotIn(value, output.getvalue())


if __name__ == "__main__":
    unittest.main()
