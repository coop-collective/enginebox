"""Template validation, safe imports and installation contracts; no real host access."""
from copy import deepcopy
from dataclasses import replace
import io
import json
import os
from pathlib import Path
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from enginebox import GuestConfig
import templates


def manifest(**overrides):
    return {"schema_version": 1, "id": "native-dev", "revision": 1, "label": "Native development",
            "minimum": {"cpu": 2, "memory_gib": 2, "disk_gib": 24},
            "recommended": {"cpu": 4, "memory_gib": 4, "disk_gib": 40}, **overrides}


def host(**overrides):
    return SimpleNamespace(id="primary", target="example-host", project="enginebox", pool="pool", network="bridge",
                           reserve_disk_gib=10, reserve_memory_gib=4, reserve_cpu=2, max_instances=16, templates=(),
                           guest=GuestConfig(1001, 1001, "agent", "/home/agent", "/workspace", ":99"),
                           **overrides)


class ManifestTests(unittest.TestCase):
    def test_normalization_is_stable_and_has_no_arbitrary_payload(self):
        normalized = templates.validate_manifest(manifest())
        self.assertEqual(templates.validate_manifest(json.dumps(normalized)), normalized)
        self.assertEqual(normalized["native_clients"], ["claude", "codex", "cursor"])
        self.assertEqual(normalized["source_image"], "images:ubuntu/24.04/cloud")
        for extra in [{"shell": "curl example | sh"}, {"files": {"auth.json": "secret"}},
                      {"config": {"api_key": "secret"}}, {"native_clients": ["unreviewed"]},
                      {"recipe": "external-shell"}, {"source_image": "images:ubuntu/26.04/cloud"}]:
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                templates.validate_manifest(manifest(**extra))

    def test_unknown_duplicate_oversized_and_non_json_inputs_rejected(self):
        bad = [b'{"id":"one","id":"two"}', b'\xff', b'x' * (templates.MAX_MANIFEST_BYTES + 1),
               '[]', '{"cpu": NaN}', manifest(schema_version=True), manifest(revision=True),
               manifest(id="../outside"), manifest(label="one\ntwo")]
        for value in bad:
            with self.subTest(value=str(value)[:60]), self.assertRaises(ValueError):
                templates.validate_manifest(value)

    def test_custom_resources_are_complete_integer_requests_above_minimum(self):
        normalized = templates.validate_manifest(manifest())
        self.assertEqual(templates.resolve_resources(normalized), normalized["recommended"])
        self.assertEqual(templates.resolve_resources(normalized, preset="minimum"), normalized["minimum"])
        custom = {"cpu": 6, "memory_gib": 10, "disk_gib": 100}
        self.assertEqual(templates.resolve_resources(normalized, custom), custom)
        for change in [{"cpu": 1}, {"memory_gib": 1}, {"disk_gib": 23}, {"cpu": True}, {"cpu": 2.0}]:
            with self.subTest(change=change), self.assertRaises(ValueError):
                templates.resolve_resources(normalized, custom | change)
        with self.assertRaises(ValueError):
            templates.resolve_resources(normalized, {"cpu": 4})
        with self.assertRaises(ValueError):
            templates.validate_manifest(manifest(recommended={"cpu": 1, "memory_gib": 4, "disk_gib": 40}))

    def test_desktop_computer_dependency(self):
        with self.assertRaisesRegex(ValueError, "requires desktop"):
            templates.validate_manifest(manifest(config={"guest_computer_mcp": True}))
        parsed = templates.validate_manifest(manifest(native_clients=[], config={"browser": False, "desktop": True}))
        self.assertEqual(parsed["native_clients"], [])
        self.assertFalse(parsed["config"]["browser"])


class RegistryTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.registry = templates.Registry(self.directory.name)

    def test_persistent_registration_and_revision_immutability(self):
        record = self.registry.register(manifest())
        self.assertEqual(self.registry.register(json.dumps(manifest()))["manifest_digest"], record["manifest_digest"])
        with self.assertRaisesRegex(ValueError, "greater"):
            self.registry.register(manifest(label="Changed"))
        updated = self.registry.register(manifest(revision=2, label="Updated"))
        self.assertNotEqual(record["manifest_digest"], updated["manifest_digest"])
        reopened = templates.Registry(self.directory.name)
        self.assertEqual(reopened.list()[0]["revision"], 2)
        self.assertEqual(len(list((self.registry.root / "history").glob("*.json"))), 2)
        for path in self.registry.root.rglob("*.json"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)

    def test_install_receipt_binding_and_revision_sync(self):
        first = self.registry.register(manifest())
        with patch("templates.install_template", return_value={"source": "ebox-template-one", "snapshot": "prepared-one", "network": "bridge"}):
            receipt = self.registry.install(host(), "native-dev")
        self.assertEqual(receipt["manifest_digest"], first["manifest_digest"])
        self.assertEqual(receipt["host_binding_digest"], templates.host_binding_digest(host()))
        self.assertTrue(self.registry.get("native-dev")["deployments"][0]["in_sync"])
        self.assertNotIn("target", self.registry.get("native-dev")["deployments"][0])
        self.assertNotIn("host_binding", self.registry.get("native-dev")["deployments"][0])
        self.assertEqual(self.registry.deployments("primary")[0]["target"], "example-host")
        self.registry.register(manifest(revision=2))
        self.assertFalse(self.registry.get("native-dev")["deployments"][0]["in_sync"])
        changed = host()
        changed.guest = replace(changed.guest, uid=2000)
        self.assertNotEqual(receipt["host_binding_digest"], templates.host_binding_digest(changed))
        changed = host()
        changed.target = "different-machine"
        self.assertNotEqual(receipt["host_binding_digest"], templates.host_binding_digest(changed))

    def test_registering_new_revision_during_install_does_not_relabel_deployed_base(self):
        first = self.registry.register(manifest())
        def installing(_host, supplied):
            self.assertEqual(supplied["revision"], 1)
            self.registry.register(manifest(revision=2))
            return {"source": "ebox-template-one", "snapshot": "prepared-one", "network": "bridge"}
        with patch("templates.install_template", side_effect=installing):
            receipt = self.registry.install(host(), "native-dev")
        self.assertEqual(receipt["manifest_digest"], first["manifest_digest"])
        self.assertEqual(receipt["revision"], 1)
        self.assertFalse(self.registry.get("native-dev")["deployments"][0]["in_sync"])

    def test_failed_install_never_publishes_receipt(self):
        self.registry.register(manifest())
        with patch("templates.install_template", side_effect=RuntimeError("preparation failed")):
            with self.assertRaisesRegex(RuntimeError, "preparation failed"):
                self.registry.install(host(), "native-dev")
        self.assertEqual(self.registry.deployments(), [])

    def test_atomic_record_failure_preserves_previous_revision(self):
        original = self.registry.register(manifest())
        with patch("templates.os.replace", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.registry.register(manifest(revision=2))
        self.assertEqual(self.registry.get("native-dev")["manifest_digest"], original["manifest_digest"])
        self.assertEqual(list(self.registry.root.rglob("*.tmp")), [])

    def test_registry_rejects_symlink_and_insecure_record(self):
        self.registry.register(manifest())
        path = self.registry.root / "manifests" / "native-dev.json"
        path.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "Unsafe"):
            self.registry.get("native-dev")
        path.unlink()
        path.symlink_to(Path(self.directory.name) / "absent")
        with self.assertRaises(OSError):
            self.registry.get("native-dev")

    def test_install_lock_excludes_second_registry_process_handle(self):
        self.registry.register(manifest())
        other = templates.Registry(self.directory.name)
        key = "install-" + templates._digest([host().id, templates.host_binding(host()), "native-dev"])
        with self.registry._lock(key, blocking=False), patch("templates.install_template") as installer:
            with self.assertRaisesRegex(ValueError, "already running"):
                other.install(host(), "native-dev")
            installer.assert_not_called()


class ImportTests(unittest.TestCase):
    def connection(self, *, status=200, headers=None, chunks=None):
        response = Mock()
        response.status = status
        response.getheader.side_effect = lambda name, default=None: (headers or {}).get(name, default)
        response.read1.side_effect = [json.dumps(manifest()).encode(), b""] if chunks is None else chunks
        connection = Mock()
        connection.getresponse.return_value = response
        return connection

    def fetch(self, connection):
        return (patch("templates.socket.getaddrinfo", return_value=[(2, 1, 6, "", ("93.184.216.34", 443))]),
                patch("templates._PinnedHTTPSConnection", return_value=connection))

    def test_https_public_address_pinned_and_proxy_environment_ignored(self):
        connection = self.connection()
        resolver, transport = self.fetch(connection)
        with resolver as dns, transport as client, patch.dict(os.environ, {"HTTPS_PROXY": "http://evil.invalid:8080"}):
            value = templates.fetch_manifest("https://templates.example/native.json")
        self.assertEqual(templates.validate_manifest(value)["id"], "native-dev")
        self.assertEqual(client.call_args.args[:3], ("templates.example", "93.184.216.34", 443))
        dns.assert_called_once()
        connection.close.assert_called_once()
        self.assertEqual(connection.request.call_args.args[:2], ("GET", "/native.json"))

    def test_credentials_redirects_private_dns_and_non_https_never_import(self):
        for url in ["http://templates.example/a", "file:///etc/passwd", "https://u:p@templates.example/a",
                    "https://templates.example:8443/a", "https://templates.example/a#fragment"]:
            with self.subTest(url=url), patch("templates.socket.getaddrinfo") as dns, self.assertRaises(ValueError):
                templates.fetch_manifest(url)
            dns.assert_not_called()
        with patch("templates.socket.getaddrinfo", return_value=[(2, 1, 6, "", ("127.0.0.1", 443))]), \
                patch("templates._PinnedHTTPSConnection") as client:
            with self.assertRaisesRegex(ValueError, "public Internet"):
                templates.fetch_manifest("https://private.example/a")
            client.assert_not_called()
        connection = self.connection(status=302, headers={"Location": "https://other.example/secret"})
        resolver, transport = self.fetch(connection)
        with resolver, transport, self.assertRaisesRegex(ValueError, "redirects"):
            templates.fetch_manifest("https://templates.example/a")
        self.assertEqual(connection.request.call_count, 1)
        connection.close.assert_called_once()

    def test_bounded_body_compression_and_deadline(self):
        for options in [{"headers": {"Content-Length": str(templates.MAX_MANIFEST_BYTES + 1)}},
                        {"headers": {"Content-Encoding": "gzip"}},
                        {"chunks": [b"x" * (templates.MAX_MANIFEST_BYTES + 1)]}]:
            connection = self.connection(**options)
            resolver, transport = self.fetch(connection)
            with resolver, transport, self.assertRaises(ValueError):
                templates.fetch_manifest("https://templates.example/a")
            connection.close.assert_called_once()
        resolver, transport = self.fetch(self.connection())
        with resolver, transport as client, patch("templates.time.monotonic", side_effect=[0, templates.IMPORT_TIMEOUT + 1]):
            with self.assertRaisesRegex(ValueError, "timed out"):
                templates.fetch_manifest("https://templates.example/a")
            client.assert_not_called()


class InstallerTests(unittest.TestCase):
    def test_explicit_network_avoids_discovery_and_config_uses_expandable_minimum_disk(self):
        machine = host()
        machine.box = Mock()
        self.assertEqual(templates.discover_network(machine), "bridge")
        machine.box.assert_not_called()
        recipe = templates.validate_manifest(manifest())
        config = templates.bootstrap_configuration(machine, recipe, "bridge")
        self.assertEqual(config["workspace"]["disk"], "24GiB")
        self.assertEqual(config["workspace"]["memory"], "4GiB")
        self.assertEqual(config["workspace"]["state_disk"], "6GiB")
        self.assertFalse(config["host"]["install_packages"])
        self.assertEqual(config["storage"], {"mode": "existing", "pool": "pool", "size": "120GiB"})
        self.assertEqual(config["network"]["mode"], "existing")
        updated = templates.bootstrap_configuration(machine, templates.validate_manifest(manifest(revision=2)), "bridge")
        self.assertNotEqual(config["workspace"]["name"], updated["workspace"]["name"])

    def test_bridge_parent_discovery_and_ambiguous_network_refusal(self):
        machine = host()
        machine.network = ""
        machine.templates = ({"source": "ebox-old"},)
        box = Mock()
        machine.box = Mock(return_value=box)
        vm = {"expanded_devices": {"eth0": {"type": "nic", "nictype": "bridged", "parent": "bridge"}}}
        box.run.return_value = json.dumps(vm).encode()
        self.assertEqual(templates.discover_network(machine), "bridge")
        self.assertIn("project=enginebox", box.run.call_args.args[1])
        vm["expanded_devices"]["eth1"] = {"type": "nic", "network": "other"}
        box.run.return_value = json.dumps(vm).encode()
        with self.assertRaisesRegex(ValueError, "unambiguous"):
            templates.discover_network(machine)

    def test_profile_discovery_does_not_ignore_local_nic_override(self):
        machine = host()
        machine.network = ""
        machine.templates = ({"source": "ebox-old"},)
        box = Mock()
        machine.box = Mock(return_value=box)
        box.run.side_effect = [json.dumps({"profiles": ["profile"], "devices": {
            "eth0": {"type": "nic", "network": "new"}}}).encode(), json.dumps({"devices": {
                "eth0": {"type": "nic", "network": "old"}}}).encode()]
        self.assertEqual(templates.discover_network(machine), "new")

    def test_installer_refuses_host_bootstrap_and_propagates_preflight_errors(self):
        for plan in [{"ok": True, "operations": [{"action": "create_project"}]},
                     {"ok": False, "errors": ["unowned project"]}]:
            with patch("templates.bootstrap.inventory", return_value={}), \
                    patch("templates.bootstrap.build_plan", return_value=plan), \
                    patch("templates.bootstrap.apply") as apply:
                with self.assertRaises(ValueError):
                    templates.install_template(host(), templates.validate_manifest(manifest()))
                apply.assert_not_called()

    def test_installer_finalizes_exact_new_base_without_nested_fleet_lock(self):
        recipe = templates.validate_manifest(manifest())
        config = templates.bootstrap_configuration(host(), recipe, "bridge")
        source = config["workspace"]["name"]
        snapshot = "prepared-" + templates.fingerprint(config)[:12]
        with patch("templates.bootstrap.inventory", return_value={}), \
                patch("templates.bootstrap.build_plan", return_value={"ok": True, "operations": [{"action": "create_workspace"}]}), \
                patch("templates.bootstrap.apply", return_value={"ok": True}) as apply, \
                patch("templates.bootstrap.run_command", return_value=json.dumps({"source": source, "snapshot": snapshot}).encode()) as final:
            result = templates.install_template(host(), recipe)
        self.assertEqual(result["source"], source)
        self.assertEqual(result["snapshot"], snapshot)
        self.assertEqual(apply.call_args.args[0], config)
        self.assertIn("enginebox-host.lock", final.call_args.args[0][-1])
        self.assertNotIn("ebox-old", final.call_args.args[0][-1])

    def test_finalizer_rejects_unowned_or_authenticated_snapshot_before_stop(self):
        config = templates.bootstrap_configuration(host(), templates.validate_manifest(manifest()), "bridge")
        source = config["workspace"]["name"]
        expected = {"project": "enginebox", "source": source, "snapshot": "prepared-one", "fingerprint": "recipe"}
        base = {"config": {"user.enginebox.owner": "enginebox", "user.enginebox.recipe": "recipe", "user.enginebox.prepared": "recipe"},
                "profiles": [source], "type": "virtual-machine", "status": "Running"}
        for altered, snapshot in [(dict(base, config={}), {"config": {"user.enginebox.clean-base": "true"}}),
                                  (base, {"config": {}}),
                                  (base, {"config": {"user.enginebox.clean-base": "true"}, "stateful": True})]:
            with patch("templates.os.open", return_value=99), \
                    patch("templates.os.fstat", return_value=SimpleNamespace(st_uid=0, st_mode=stat.S_IFREG | 0o600)), \
                    patch("templates.fcntl.flock"), patch("subprocess.check_output", side_effect=[json.dumps(altered).encode(), json.dumps(snapshot).encode()]), \
                    patch("subprocess.run") as stop, patch("sys.argv", ["script", json.dumps(expected)]):
                with self.assertRaises(SystemExit):
                    exec(compile(templates.FINALIZE_SCRIPT, "template-finalizer", "exec"), {})
                stop.assert_not_called()


if __name__ == "__main__":
    unittest.main()
