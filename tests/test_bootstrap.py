"""Bootstrap planning regression tests; no SSH, root or VM is required."""
from contextlib import nullcontext, redirect_stdout
import copy
import io
import json
import os
from pathlib import Path
import shlex
import sys
import tempfile
import tarfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "runtime"))
import bootstrap
from bootstrap_config import normalize, fingerprint, size_bytes


def configuration():
    return normalize({"host": {"target": "builder@example"},
                      "incus": {"project": "ebox-testing", "profile": "ebox-profile"},
                      "storage": {"pool": "existing-pool"},
                      "network": {"name": "existing-network"},
                      "workspace": {"name": "ebox-test"},
                      "modules": {"browser": False}})


def host_inventory(config):
    return {"system": "Linux", "architecture": "x86_64", "uid": 0, "kvm": True, "python_version": [3, 12, 3],
            "os": {"ID": "ubuntu", "VERSION_ID": "24.04"}, "cpu_count": 16,
            "memory_available": 32 * 2**30, "host_disk_free": 500 * 2**30,
            "incus_disk_free": 500 * 2**30,
            "incus_present": True,
            "server": {"environment": {"server_version": "6.0.5", "driver": "qemu | lxc"},
                       "api_extensions": ["migration_stateful"]},
            "qemu_version": "QEMU emulator version 10.2.1", "projects": [], "profiles": [], "instances": [],
            "storage": [{"name": config["storage"]["pool"], "driver": "btrfs", "config": {}}],
            "pool_resources": {"space": {"total": 200 * 2**30, "used": 10 * 2**30}},
            "networks": [{"name": config["network"]["name"], "type": "bridge", "managed": True,
                          "config": {"ipv4.address": "10.9.0.1/24"}}], "routes": [], "errors": []}


def add_owned_resources(config, observed, prepared=True):
    marker = {bootstrap.OWNER_KEY: config["incus"]["project"]}
    observed["projects"] = [{"name": config["incus"]["project"], "config": dict(marker)}]
    observed["profiles"] = [{"name": config["incus"]["profile"], **bootstrap.profile_definition(config)}]
    instance_config = {**marker, bootstrap.RECIPE_KEY: fingerprint(config)}
    if prepared:
        instance_config[bootstrap.PREPARED_KEY] = fingerprint(config)
    observed["instances"] = [{"name": config["workspace"]["name"], "config": instance_config,
                              "profiles": [config["incus"]["profile"]], "type": "virtual-machine"}]


class BootstrapConfigTests(unittest.TestCase):
    def test_minimal_config_requires_explicit_host_and_resources(self):
        with self.assertRaisesRegex(ValueError, "host.target"):
            normalize({})
        with self.assertRaisesRegex(ValueError, "storage.pool"):
            normalize({"host": {"target": "example"}})

    def test_rejects_unknown_secret_and_unimplemented_mount_sections(self):
        for key in ["credentials", "mounts", "api_key"]:
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "unsupported"):
                normalize({**configuration(), key: {"secret": "do-not-use"}})

    def test_rejects_invalid_types_instead_of_interpreting_truthy_strings(self):
        for section, key, value in [("host", "install_packages", "false"), ("workspace", "cpu", True),
                                    ("network", "mode", []), ("modules", "ci", "0")]:
            raw = configuration()
            raw[section][key] = value
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "wrong type"):
                normalize(raw)

    def test_state_disk_includes_memory_overhead(self):
        raw = configuration()
        raw["workspace"]["state_disk"] = "4GiB"
        with self.assertRaisesRegex(ValueError, "exceed configured RAM"):
            normalize(raw)

    def test_guest_computer_requires_desktop(self):
        raw = configuration()
        raw["modules"]["guest_computer_mcp"] = True
        with self.assertRaisesRegex(ValueError, "requires"):
            normalize(raw)

    def test_rejects_shell_and_path_injection(self):
        for section, key, value in [("host", "target", "example;touch /tmp/pwn"),
                                    ("incus", "project", "-option"), ("workspace", "path", "/tmp/../root"),
                                    ("workspace", "home", "/root"), ("network", "subnet", "8.8.8.1/24")]:
            raw = configuration()
            raw[section][key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                normalize(raw)

    def test_uid_and_paths_are_configurable(self):
        raw = configuration()
        raw["workspace"].update(user="builder", uid=2000, gid=2001, home="/home/builder", path="/work")
        env = bootstrap.guest_environment(normalize(raw))
        self.assertEqual((env["EBOX_USER"], env["EBOX_UID"], env["EBOX_GID"], env["EBOX_WORKSPACE"]),
                         ("builder", "2000", "2001", "/work"))

    def test_connection_alias_does_not_change_guest_recipe(self):
        first = configuration()
        second = copy.deepcopy(first)
        second["host"]["target"] = "different-alias"
        self.assertEqual(fingerprint(first), fingerprint(second))
        second["workspace"]["memory"] = "6GiB"
        self.assertNotEqual(fingerprint(first), fingerprint(second))

    def test_init_never_overwrites_existing_config(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.toml"
            with patch("bootstrap.inventory", side_effect=AssertionError("Host access")), redirect_stdout(io.StringIO()):
                bootstrap.run_cli(["init", "--output", str(path)])
                self.assertEqual(bootstrap.load_config(path)["workspace"]["name"], "ebox-dev")
                with self.assertRaises(SystemExit):
                    bootstrap.run_cli(["init", "--output", str(path)])


class BootstrapPlanTests(unittest.TestCase):
    def setUp(self):
        self.config = configuration()
        self.observed = host_inventory(self.config)
        self.no_process = patch("bootstrap.subprocess.run", side_effect=AssertionError("Planner ran a subprocess"))
        self.no_process.start()
        self.addCleanup(self.no_process.stop)

    def plan(self):
        return bootstrap.build_plan(self.config, self.observed)

    def test_existing_pool_and_network_are_referenced_without_mutation(self):
        result = self.plan()
        self.assertTrue(result["ok"], result)
        self.assertEqual([o["action"] for o in result["operations"]],
                         ["create_project", "create_profile", "create_workspace", "prepare_workspace"])

    def test_second_apply_is_noop_even_if_running_vm_consumes_available_ram(self):
        add_owned_resources(self.config, self.observed)
        self.observed["memory_available"] = 2**30
        self.assertEqual(self.plan()["operations"], [])
        self.assertTrue(self.plan()["ok"])

    def test_interrupted_preparation_resumes_only_owned_instance(self):
        add_owned_resources(self.config, self.observed, prepared=False)
        self.assertEqual(self.plan()["operations"], [{"action": "prepare_workspace"}])
        self.observed["instances"][0]["config"].pop(bootstrap.OWNER_KEY)
        self.assertFalse(self.plan()["ok"])

    def test_name_collision_does_not_adopt_existing_project(self):
        self.observed["projects"] = [{"name": self.config["incus"]["project"], "config": {}}]
        self.assertIn("without Engine Box ownership", "; ".join(self.plan()["errors"]))

    def test_config_drift_blocks_changes_to_existing_workspace(self):
        add_owned_resources(self.config, self.observed)
        self.config["workspace"]["cpu"] = 8
        errors = "; ".join(self.plan()["errors"])
        self.assertIn("profile differs", errors)
        self.assertIn("configuration has changed", errors)

    def test_added_device_blocks_stateful_assumptions(self):
        add_owned_resources(self.config, self.observed)
        self.observed["instances"][0]["devices"] = {"host": {"type": "disk", "source": "/", "path": "/host"}}
        self.assertIn("adds devices", "; ".join(self.plan()["errors"]))

    def test_missing_kvm_or_stateful_extension_is_not_supported(self):
        self.observed["kvm"] = False
        self.observed["server"]["api_extensions"] = []
        errors = "; ".join(self.plan()["errors"])
        self.assertIn("/dev/kvm", errors)
        self.assertIn("stateful", errors)

    def test_capacity_checks_include_memory_state(self):
        self.observed["memory_available"] = 4 * 2**30
        self.observed["pool_resources"]["space"] = {"total": 35 * 2**30, "used": 0}
        errors = "; ".join(self.plan()["errors"])
        self.assertIn("RAM", errors)
        self.assertIn("saved memory", errors)

    def test_managed_network_detects_route_overlap(self):
        self.config["network"] = {"mode": "managed", "name": "new-network", "subnet": "10.9.0.1/24"}
        self.observed["networks"] = []
        self.observed["routes"] = [{"dst": "default"}, {"dst": "10.9.0.0/16"}]
        self.assertIn("overlaps host route", "; ".join(self.plan()["errors"]))

    def test_missing_packages_require_explicit_optin_and_supported_distribution(self):
        self.observed.update(incus_present=False, server=None, storage=[], networks=[])
        self.config["storage"].update(mode="development-loop", size="120GiB")
        self.config["network"]["mode"] = "managed"
        self.assertFalse(self.plan()["ok"])
        self.config["host"]["install_packages"] = True
        self.assertTrue(self.plan()["ok"], self.plan())
        self.assertEqual(self.plan()["operations"][0], {"action": "install_packages"})
        self.observed["os"]["ID"] = "debian"
        self.assertFalse(self.plan()["ok"])

    def test_loop_capacity_uses_incus_filesystem_not_journal_filesystem(self):
        self.config["storage"].update(mode="development-loop", pool="new-pool")
        self.observed["incus_disk_free"] = 10 * 2**30
        self.assertIn("loop pool bound", "; ".join(self.plan()["errors"]))

    def test_existing_firewall_generates_warning_not_global_rule_operations(self):
        self.observed["firewall_tools"] = ["docker", "ufw"]
        self.assertTrue(self.plan()["ok"])
        self.assertIn("preserved", " ".join(self.plan()["warnings"]))
        self.assertFalse(any("firewall" in operation["action"] for operation in self.plan()["operations"]))


class BootstrapExecutionTests(unittest.TestCase):
    def test_credential_guard_failure_never_stops_or_labels_base(self):
        config = configuration()
        calls = []
        def fake_incus(_config, *argv, **kwargs):
            calls.append(argv)
            if argv[:1] == ("query",):
                return json.dumps({"status": "Running"}).encode()
            if bootstrap.CLEAN_BASE_CHECK in argv:
                raise RuntimeError("Credential state found")
            return b""
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory)
            for filename in ["enginebox.py", "mcp_server.py", "browser_tools.py", "requirements.lock", "guest_setup.sh"]:
                (source / filename).touch()
            with patch("bootstrap.incus", side_effect=fake_incus), patch("bootstrap.wait_guest"):
                with self.assertRaisesRegex(RuntimeError, "Credential state"):
                    bootstrap.prepare_workspace(config, source)
        self.assertFalse(any(call[0] in {"stop", "snapshot"} or call[:2] == ("config", "set") for call in calls))

    def test_clean_base_identity_reset_and_marker_are_scoped_before_start(self):
        config = configuration()
        calls = []
        def fake_incus(_config, *argv, **kwargs):
            calls.append(argv)
            if argv[:1] == ("query",):
                return b"[]" if "/snapshots?" in argv[1] else b'{"status":"Running"}'
            return b""
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory)
            for filename in ["enginebox.py", "mcp_server.py", "browser_tools.py", "requirements.lock", "guest_setup.sh"]:
                (source / filename).touch()
            with patch("bootstrap.incus", side_effect=fake_incus), patch("bootstrap.wait_guest"):
                bootstrap.prepare_workspace(config, source)
        auth = next(i for i, call in enumerate(calls) if bootstrap.CLEAN_BASE_CHECK in call)
        identity = next(i for i, call in enumerate(calls) if any("Path('/etc/machine-id').write_text('')" in arg for arg in call))
        stop = next(i for i, call in enumerate(calls) if call[0] == "stop")
        snapshot = next(i for i, call in enumerate(calls) if call[0] == "snapshot")
        unset = max(i for i, call in enumerate(calls) if call[:2] == ("config", "unset"))
        start = next(i for i, call in enumerate(calls) if call[0] == "start")
        self.assertLess(auth, identity)
        self.assertLess(identity, stop)
        self.assertLess(stop, snapshot)
        self.assertLess(snapshot, unset)
        self.assertLess(unset, start)

    def test_failed_final_restart_does_not_mark_preparation_complete(self):
        config = configuration()
        calls = []
        def fake_incus(_config, *argv, **kwargs):
            calls.append(argv)
            if argv[:1] == ("query",):
                return b"[]" if "/snapshots?" in argv[1] else b'{"status":"Running"}'
            return b""
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory)
            for filename in ["enginebox.py", "mcp_server.py", "browser_tools.py", "requirements.lock", "guest_setup.sh"]:
                (source / filename).touch()
            with patch("bootstrap.incus", side_effect=fake_incus), patch("bootstrap.wait_guest", side_effect=[None, RuntimeError("restart failed")]):
                with self.assertRaisesRegex(RuntimeError, "restart failed"):
                    bootstrap.prepare_workspace(config, source)
        self.assertFalse(any(any(arg.startswith(bootstrap.PREPARED_KEY + "=") for arg in call) for call in calls))
        self.assertTrue(any(call[:2] == ("config", "unset") for call in calls))

    def test_browser_enables_mcp_without_enabling_computer_tools(self):
        config = configuration()
        config["modules"]["browser"] = True
        env = bootstrap.guest_environment(config)
        self.assertEqual(env["EBOX_MCP"], "1")
        self.assertEqual(env["EBOX_COMPUTER_MCP"], "0")

    def test_clean_base_guard_rejects_auth_without_printing_secrets(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"EBOX_HOME": directory}):
            home = Path(directory)
            exec(bootstrap.CLEAN_BASE_CHECK, {})
            (home / ".claude.json").write_text(json.dumps({"nested": {"refreshToken": "SECRET-NOT-PRINTED"}}))
            with self.assertRaises(SystemExit) as stopped:
                exec(bootstrap.CLEAN_BASE_CHECK, {})
            self.assertIn(".claude.json", str(stopped.exception))
            self.assertNotIn("SECRET-NOT-PRINTED", str(stopped.exception))

    def test_clean_base_guard_rejects_used_browser_profile(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {"EBOX_HOME": directory}):
            profile = Path(directory) / ".local/share/enginebox/browser/codex/profile"
            profile.mkdir(parents=True)
            exec(bootstrap.CLEAN_BASE_CHECK, {})
            (profile / "Cookies").write_bytes(b"test-cookie-store")
            with self.assertRaisesRegex(SystemExit, "profile"):
                exec(bootstrap.CLEAN_BASE_CHECK, {})

    def test_wheel_bundle_uses_installed_data_files_and_excludes_arbitrary_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, prefix = root / "lib", root / "prefix"
            source.mkdir()
            data = prefix / "share/enginebox"
            data.mkdir(parents=True)
            for name in bootstrap.PYTHON_ASSETS:
                (source / name).write_text("# test module\n")
            for name in bootstrap.DATA_ASSETS:
                (data / name).write_text("test data\n")
            (source / "private-config.json").write_text('{"secret": "excluded"}')
            with patch("bootstrap.__file__", str(source / "bootstrap.py")), patch("bootstrap.sys.prefix", str(prefix)):
                bundle = bootstrap.make_bundle(configuration())
            with tarfile.open(fileobj=io.BytesIO(bundle), mode="r:gz") as archive:
                names = archive.getnames()
                self.assertIn("guest_setup.sh", names)
                self.assertIn("requirements.lock", names)
                self.assertNotIn("private-config.json", names)

    def test_remote_command_is_argv_safe(self):
        config = configuration()
        command = bootstrap.transport_command(config, ["python3", "-c", "print('literal $HOME')"])
        self.assertEqual(command[-2], "builder@example")
        self.assertEqual(shlex.split(command[-1]), ["sudo", "-n", "python3", "-c", "print('literal $HOME')"])

    def test_blocked_apply_does_not_upload_or_mutate(self):
        config = configuration()
        observed = host_inventory(config)
        observed["kvm"] = False
        with patch("bootstrap.inventory", return_value=observed), patch("bootstrap.make_bundle") as bundle, patch("bootstrap.subprocess.run") as run:
            with self.assertRaisesRegex(RuntimeError, "Preflight failed"):
                bootstrap.apply(config)
            bundle.assert_not_called()
            run.assert_not_called()

    def test_noop_apply_does_not_upload_or_mutate(self):
        config = configuration()
        observed = host_inventory(config)
        add_owned_resources(config, observed)
        with patch("bootstrap.inventory", return_value=observed), patch("bootstrap.make_bundle") as bundle:
            self.assertFalse(bootstrap.apply(config)["changed"])
            bundle.assert_not_called()

    def test_apply_rechecks_after_each_operation_under_lock(self):
        config = configuration()
        first = host_inventory(config)
        add_owned_resources(config, first, prepared=False)
        final = copy.deepcopy(first)
        final["instances"][0]["config"][bootstrap.PREPARED_KEY] = fingerprint(config)
        with patch("bootstrap.os.geteuid", return_value=0), patch("bootstrap.host_lock", return_value=nullcontext()), \
             patch("bootstrap.inventory", side_effect=[first, final]) as observe, \
             patch("bootstrap.execute_operation") as execute, patch("bootstrap.write_journal"):
            result = bootstrap.apply_local(config, Path("/tmp/runtime"))
        self.assertEqual(result["completed"], ["prepare_workspace"])
        self.assertEqual(observe.call_count, 2)
        execute.assert_called_once_with(config, "prepare_workspace", Path("/tmp/runtime"))


if __name__ == "__main__":
    unittest.main()
