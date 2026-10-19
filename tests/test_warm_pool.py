"""Warm assignment and refill regressions using an in-memory Incus boundary."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from copy import deepcopy
from datetime import datetime, timedelta, timezone
import ast
import json
from pathlib import Path
import re
import subprocess
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from enginebox import GuestConfig
from fleet import INVENTORY_SCRIPT, load_hosts
from warm_pool import PREFIX, WarmPool, eligible, generation


GIB = 1024 ** 3


class MemoryFleet:
    """Fake only VM storage/transport and Fleet helper boundaries, never WarmPool."""

    def __init__(self):
        self.host = SimpleNamespace(
            id="primary", project="test-project", pool="pool", disk_gib=30,
            guest=GuestConfig(1001, 1001, "agent", "/home/agent", "/workspace", ":99"),
            warm_pools=({"template": "native", "size": "small", "ready": 1},))
        self.host.box = lambda: self
        self.hosts = {self.host.id: self.host}
        self.locks = {self.host.id: threading.Lock()}
        self.template_value = {"id": "native", "label": "Native", "source": "ebox-base", "snapshot": "prepared"}
        self.snapshot = {"name": "ebox-base/prepared", "created_at": "2026-10-19T00:00:00Z",
                         "last_used_at": "2026-10-19T00:00:00Z", "config": {"user.enginebox.clean-base": "true"}}
        self.vms = {}
        self.calls = []
        self.resume_error = None
        self.readiness_error = None
        self.cold_error = None
        self.on_ready = None
        self.host_lock_depth = 0
        self.budget_available = True

    def template(self, host, template_id):
        if template_id != self.template_value["id"]:
            raise ValueError("Unknown template")
        return deepcopy(self.template_value)

    def resources(self, host, template, size=None, resources=None):
        if resources:
            return dict(resources)
        return {"cpu": 2, "memory_gib": 2, "disk_gib": host.disk_gib}

    def template_snapshot(self, host, template):
        return deepcopy(self.snapshot)

    def expected_generation(self):
        return generation(self.host, self.template_value,
                          self.resources(self.host, self.template_value), self.snapshot)

    def add_member(self, name="ebox-warm-one", **overrides):
        record = {"name": name, "project": self.host.project, "pool": self.host.pool,
                  "managed": True, "type": "virtual-machine", "template": "native",
                  "status": "Frozen", "cpu": 2, "memory_bytes": 2 * GIB,
                  "disk_bytes": 30 * GIB, "state_disk_bytes": 4 * GIB,
                  "warm_pool_state": "ready", "pool_generation": self.expected_generation(),
                  "pool_size": "small", "pool_created_at": datetime.now(timezone.utc).isoformat(),
                  "pool_claim": None, "workspace_name": None}
        record.update(overrides)
        self.vms[name] = record
        return record

    def inventory(self, host):
        return {"instances": deepcopy(list(self.vms.values()))}

    def check_name(self, host, name, facts):
        if any(vm["name"] == name or vm.get("workspace_name") == name for vm in facts["instances"]):
            raise ValueError("Workspace already exists")

    def run(self, *argv):
        self.calls.append(argv)
        if argv[:2] != ("config", "set"):
            raise AssertionError(f"Unexpected transport call: {argv!r}")
        vm = self.vms[argv[2]]
        for argument in argv[3:]:
            if argument == "--property":
                continue
            key, value = argument.split("=", 1)
            if key == "description":
                if not value.startswith(PREFIX):
                    raise AssertionError("Missing pool description prefix")
                payload = json.loads(value[len(PREFIX):])
                vm["description"] = value
                vm["pool_claim"] = payload
                vm["warm_pool_state"] = payload["state"]
                vm["workspace_name"] = payload.get("workspace_name")
            elif key == "user.enginebox.pool.state":
                if vm["status"] == "Frozen":
                    raise AssertionError("user.* changes must not wait on a frozen guest")
                vm["warm_pool_state"] = value
            elif key == "user.enginebox.workspace":
                vm["workspace_name"] = value
            else:
                raise AssertionError(f"Unexpected config key: {key}")
        return b""

    def lifecycle(self, name, action):
        self.calls.append(("lifecycle", name, action))
        vm = self.vms[name]
        if action == "resume":
            if not vm["pool_claim"] or vm["pool_claim"]["state"] != "claimed":
                raise AssertionError("A persistent claim must precede resume")
            if self.resume_error:
                raise self.resume_error
            vm["status"] = "Running"
        elif action == "pause":
            if vm["warm_pool_state"] != "ready" or vm["status"] != "Running":
                raise AssertionError("Publish readiness while running, before freeze")
            vm["status"] = "Frozen"
        else:
            raise AssertionError(f"Unexpected lifecycle action: {action}")
        return {"name": name, "status": vm["status"]}

    def _wait_ready(self, box, name):
        self.calls.append(("ready", name))
        if self.on_ready:
            self.on_ready(name)
        if self.readiness_error:
            raise self.readiness_error
        return {"ready": True, "identity": True, "dns": True}

    def _create(self, host, name, template, size, *, resources=None, pool_metadata=None, wait=True):
        self.calls.append(("create", name, template, size, pool_metadata, wait))
        if self.cold_error:
            raise self.cold_error
        if not self.budget_available:
            raise ValueError("No additional budget")
        vm = self.add_member(name, status="Running", warm_pool_state=None)
        if pool_metadata:
            vm.update(warm_pool_state=pool_metadata["state"], pool_generation=pool_metadata["generation"],
                      pool_size=pool_metadata["size"], pool_created_at=pool_metadata["created_at"])
        return {"name": name, "status": "Running", "readiness": {"ready": True}}

    @contextmanager
    def host_lock(self, host):
        self.host_lock_depth += 1
        try:
            yield
        finally:
            self.host_lock_depth -= 1


class WarmPoolTests(unittest.TestCase):
    def setUp(self):
        self.fleet = MemoryFleet()
        self.host = self.fleet.host
        self.pool = WarmPool(self.fleet)
        self.inventory_patch = patch("fleet.inventory", side_effect=self.fleet.inventory)
        self.inventory_patch.start()
        self.addCleanup(self.inventory_patch.stop)
        self.host_lock_patch = patch("fleet.host_operation_lock", side_effect=self.fleet.host_lock)
        self.host_lock_patch.start()
        self.addCleanup(self.host_lock_patch.stop)
        self.capacity_patch = patch("fleet.resource_fits", create=True,
                                    side_effect=lambda *_: self.fleet.budget_available)
        self.capacity_patch.start()
        self.addCleanup(self.capacity_patch.stop)

    def allocate(self, name="ebox-request", pool=None):
        # Production Fleet owns these locks; WarmPool's documented contract
        # requires the same boundary when called directly in a unit test.
        with self.fleet.locks[self.host.id], self.fleet.host_lock(self.host):
            return (pool or self.pool).allocate(self.host, name, "native", "small", job_id="a" * 32)

    def test_warm_claim_uses_existing_budget_and_preserves_physical_name(self):
        self.fleet.add_member()
        self.fleet.budget_available = False
        result = self.allocate()
        self.assertEqual((result["name"], result["workspace_name"], result["startup"]),
                         ("ebox-warm-one", "ebox-request", "warm"))
        self.assertFalse(any(call[0] == "create" for call in self.fleet.calls))
        self.assertTrue(self.pool.events[self.host.id].is_set())
        claim = self.fleet.vms["ebox-warm-one"]["pool_claim"]
        self.assertEqual((claim["workspace_name"], claim["job_id"]), ("ebox-request", "a" * 32))
        self.assertEqual(claim["generation"], self.fleet.expected_generation())
        self.assertIn("description=", self.fleet.calls[0][-1])

    def test_two_concurrent_claims_get_one_warm_vm_and_one_explicit_cold_fallback(self):
        self.fleet.add_member()
        barrier = threading.Barrier(2)
        def request(name):
            barrier.wait(timeout=3)
            return self.allocate(name)
        with ThreadPoolExecutor(max_workers=2) as workers:
            results = list(workers.map(request, ["ebox-first", "ebox-second"]))
        self.assertEqual(sorted(result["startup"] for result in results), ["cold", "warm"])
        self.assertEqual(len({result["name"] for result in results}), 2)
        claims = [call for call in self.fleet.calls if call[:3] == ("config", "set", "ebox-warm-one")
                  and "--property" in call]
        self.assertEqual(len(claims), 1)

    def test_failed_resume_persists_claim_and_restart_cannot_reuse_it(self):
        self.fleet.add_member()
        self.fleet.resume_error = RuntimeError("resume failed")
        with self.assertRaisesRegex(RuntimeError, "ebox-warm-one.*never return"):
            self.allocate()
        self.assertEqual(self.fleet.vms["ebox-warm-one"]["status"], "Frozen")
        self.fleet.resume_error = None
        restarted = WarmPool(self.fleet)
        result = self.allocate("ebox-second", restarted)
        self.assertEqual(result["startup"], "cold")
        with self.assertRaisesRegex(ValueError, "already exists"):
            self.allocate("ebox-request", restarted)

    def test_failed_readiness_keeps_running_claim_excluded(self):
        self.fleet.add_member()
        self.fleet.readiness_error = RuntimeError("DNS unavailable")
        with self.assertRaisesRegex(RuntimeError, "Workspace reserved"):
            self.allocate()
        self.assertEqual(self.fleet.vms["ebox-warm-one"]["status"], "Running")
        self.fleet.readiness_error = None
        self.assertEqual(self.allocate("ebox-second")["startup"], "cold")

    def test_lost_claim_response_does_not_make_committed_reservation_reusable(self):
        self.fleet.add_member()
        original = self.fleet.run
        def interrupted_response(*argv):
            original(*argv)
            raise RuntimeError("SSH response lost after metadata commit")
        with patch.object(self.fleet, "run", side_effect=interrupted_response):
            with self.assertRaisesRegex(RuntimeError, "response lost"):
                self.allocate()
        self.assertEqual(self.fleet.vms["ebox-warm-one"]["pool_claim"]["state"], "claimed")
        self.assertEqual(self.allocate("ebox-second", WarmPool(self.fleet))["startup"], "cold")

    def test_mismatched_generation_or_real_resources_are_never_claimed(self):
        resource_map = self.fleet.resources(self.host, self.fleet.template_value)
        for field, changed in [("pool_generation", "old"), ("status", "Running"),
                               ("status", "Stopped"), ("pool", "other-pool"),
                               ("project", "other-project"), ("managed", False),
                               ("cpu", 4), ("memory_bytes", GIB), ("disk_bytes", 31 * GIB),
                               ("state_disk_bytes", 3 * GIB), ("warm_pool_state", "preparing"),
                               ("pool_claim", {"state": "claimed"})]:
            with self.subTest(field=field, changed=changed):
                vm = self.fleet.add_member(**{field: changed})
                self.assertFalse(eligible(vm, self.host, "native", resource_map, self.fleet.expected_generation()))

    def test_same_named_replacement_invalidates_generation_but_last_use_does_not(self):
        resources = self.fleet.resources(self.host, self.fleet.template_value)
        original = self.fleet.expected_generation()
        later_use = self.fleet.snapshot | {"last_used_at": "2026-10-20T00:00:00Z"}
        self.assertEqual(original, generation(self.host, self.fleet.template_value, resources, later_use))
        replaced = self.fleet.snapshot | {"created_at": "2026-10-20T00:00:00Z"}
        self.assertNotEqual(original, generation(self.host, self.fleet.template_value, resources, replaced))

    def test_generation_binds_effective_guest_config_and_ignores_observational_fields(self):
        resources = self.fleet.resources(self.host, self.fleet.template_value)
        original = self.fleet.expected_generation()
        display_only = self.fleet.template_value | {"label": "A clearer display label"}
        observed = self.fleet.snapshot | {"size": 5000000, "expires_at": "2026-12-01T00:00:00Z"}
        self.assertEqual(original, generation(self.host, display_only, resources, observed))
        for field, value in [("config", {"security.secureboot": "false"}),
                             ("expanded_config", {"limits.cpu": "4"}),
                             ("expanded_devices", {"eth0": {"network": "other-bridge"}}),
                             ("profiles", ["different-profile"]), ("architecture", "aarch64"),
                             ("stateful", True)]:
            with self.subTest(field=field):
                self.assertNotEqual(original, generation(self.host, self.fleet.template_value,
                                                         resources, self.fleet.snapshot | {field: value}))

    def test_refill_releases_both_locks_during_readiness_and_marks_before_pause(self):
        def verify_locks(name):
            self.assertFalse(self.fleet.locks[self.host.id].locked())
            self.assertEqual(self.fleet.host_lock_depth, 0)
            self.assertEqual(self.fleet.vms[name]["warm_pool_state"], "preparing")
        self.fleet.on_ready = verify_locks
        self.assertTrue(self.pool.reconcile_once(self.host))
        vm = next(iter(self.fleet.vms.values()))
        self.assertEqual((vm["warm_pool_state"], vm["status"]), ("ready", "Frozen"))
        self.assertFalse(self.pool.reconcile_once(self.host))
        creation = next(call for call in self.fleet.calls if call[0] == "create")
        self.assertFalse(creation[-1])

    def test_existing_ready_member_can_be_claimed_while_replacement_waits(self):
        self.host.warm_pools[0]["ready"] = 2
        self.fleet.add_member()
        allocations = []
        def claim_during_boot(name):
            self.fleet.on_ready = None
            allocations.append(self.allocate("ebox-during-boot"))
        self.fleet.on_ready = claim_during_boot
        self.assertTrue(self.pool.reconcile_once(self.host))
        self.assertEqual(allocations[0]["startup"], "warm")
        self.assertEqual(allocations[0]["name"], "ebox-warm-one")
        self.assertEqual(sum(vm["status"] == "Frozen" for vm in self.fleet.vms.values()), 1)

    def test_preparing_counts_toward_target_and_capacity_failure_does_not_create(self):
        self.fleet.add_member(status="Running", warm_pool_state="preparing")
        self.assertFalse(self.pool.reconcile_once(self.host))
        self.fleet.vms.clear()
        self.fleet.budget_available = False
        self.assertFalse(self.pool.reconcile_once(self.host))
        self.assertIn("capacity", self.pool.errors[(self.host.id, "native", "small")])
        self.assertFalse(any(call[0] == "create" for call in self.fleet.calls))

    def test_stale_ready_and_interrupted_preparing_are_quarantined_without_refill_loop(self):
        stale_time = (datetime.now(timezone.utc) - timedelta(minutes=11)).isoformat()
        for overrides in [{"status": "Running"}, {"status": "Stopped"},
                          {"status": "Running", "warm_pool_state": "preparing", "pool_created_at": stale_time}]:
            with self.subTest(overrides=overrides):
                self.fleet.vms.clear()
                self.fleet.calls.clear()
                self.fleet.add_member(**overrides)
                self.assertFalse(self.pool.reconcile_once(self.host))
                self.assertEqual(self.fleet.vms["ebox-warm-one"]["pool_claim"]["state"], "quarantined")
                self.assertFalse(any(call[0] == "create" for call in self.fleet.calls))

    def test_failed_preparation_is_quarantined_and_remains_accounted(self):
        self.fleet.readiness_error = RuntimeError("guest failed to boot")
        with self.assertRaisesRegex(RuntimeError, "guest failed"):
            self.pool.reconcile_once(self.host)
        vm = next(iter(self.fleet.vms.values()))
        self.assertEqual(vm["pool_claim"]["state"], "quarantined")
        self.fleet.readiness_error = None
        self.assertFalse(self.pool.reconcile_once(self.host))
        self.assertEqual(len(self.fleet.vms), 1)

    def test_reconcile_yields_when_an_allocation_holds_the_controller_lock(self):
        with self.fleet.locks[self.host.id]:
            self.assertFalse(self.pool.reconcile_once(self.host))
        self.assertEqual(self.fleet.calls, [])

    def test_bad_first_policy_does_not_block_later_valid_pool(self):
        self.host.warm_pools = ({"template": "uninstalled", "size": "small", "ready": 1},
                                {"template": "native", "size": "small", "ready": 1})
        original_resources = self.fleet.resources
        for phase in ("template", "resources", "snapshot", "snapshot-timeout"):
            with self.subTest(phase=phase):
                self.fleet.vms.clear()
                self.pool.errors.clear()
                def template(_host, template_id):
                    if template_id == "uninstalled" and phase == "template":
                        raise ValueError("Template not installed")
                    return self.fleet.template_value | {"id": template_id}
                def resources(host, source, size=None, custom=None):
                    if source["id"] == "uninstalled" and phase == "resources":
                        raise ValueError("Resource preset is below minimum")
                    return original_resources(host, source, size, custom)
                def snapshot(_host, source):
                    if source["id"] == "uninstalled":
                        if phase == "snapshot-timeout":
                            raise subprocess.TimeoutExpired("snapshot query", 10)
                        if phase == "snapshot":
                            raise RuntimeError("Snapshot no longer exists")
                    return deepcopy(self.fleet.snapshot)
                with patch.object(self.fleet, "template", side_effect=template), \
                     patch.object(self.fleet, "resources", side_effect=resources), \
                     patch.object(self.fleet, "template_snapshot", side_effect=snapshot):
                    self.assertTrue(self.pool.reconcile_once(self.host))
                    records = self.pool.summary(self.host, self.fleet.inventory(self.host))
                self.assertIn("error", records[0])
                self.assertEqual(records[1]["ready"], 1)
                self.assertNotIn("error", records[1])
                self.assertIn((self.host.id, "uninstalled", "small"), self.pool.errors)
                self.assertNotIn((self.host.id, "native", "small"), self.pool.errors)
                self.assertEqual(self.pool.host_errors, {})

    def test_one_size_success_does_not_clear_another_sizes_capacity_error(self):
        self.host.warm_pools = ({"template": "native", "size": "large", "ready": 1},
                                {"template": "native", "size": "small", "ready": 1})
        def resources(_host, _template, size=None, _custom=None):
            return {"cpu": 4 if size == "large" else 2, "memory_gib": 4 if size == "large" else 2, "disk_gib": 30}
        with patch.object(self.fleet, "resources", side_effect=resources), \
             patch("fleet.resource_fits", side_effect=lambda _host, _facts, selected: selected["cpu"] == 2):
            self.assertTrue(self.pool.reconcile_once(self.host))
            records = self.pool.summary(self.host, self.fleet.inventory(self.host))
            self.assertIn("capacity", records[0]["error"])
            self.assertNotIn("error", records[1])
            self.assertEqual(records[1]["ready"], 1)
            large = resources(self.host, self.fleet.template_value, "large")
            self.fleet.add_member("ebox-warm-large", cpu=4, memory_bytes=4 * GIB, state_disk_bytes=6 * GIB,
                pool_size="large", pool_generation=generation(self.host, self.fleet.template_value, large, self.fleet.snapshot))
            self.assertFalse(self.pool.reconcile_once(self.host))
            recovered = self.pool.summary(self.host, self.fleet.inventory(self.host))
            self.assertTrue(all("error" not in record for record in recovered))

    def test_background_loop_keeps_preparation_failure_scoped_to_its_policy(self):
        self.host.warm_pools = ({"template": "native", "size": "small", "ready": 1},
                                {"template": "native", "size": "recommended", "ready": 1})
        self.fleet.readiness_error = RuntimeError("Fixture guest failed readiness")
        with patch.object(self.pool.events[self.host.id], "wait", side_effect=lambda _seconds: self.pool.stopping.set()):
            self.pool._loop(self.host)
        self.assertEqual(self.pool.host_errors, {})
        records = self.pool.summary(self.host, self.fleet.inventory(self.host))
        self.assertIn("failed readiness", records[0]["error"])
        self.assertNotIn("error", records[1])


class WarmPoolConfigurationTests(unittest.TestCase):
    BASE = '''schema_version=1
[[hosts]]
id="primary"
target="test-server"
[[hosts.templates]]
id="native"
source="ebox-base"
snapshot="prepared"
'''

    def load(self, extra=""):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fleet.toml"
            path.write_text(self.BASE + extra)
            return load_hosts(path)[0]

    def test_default_off_and_explicit_policy_are_distinct(self):
        self.assertEqual(self.load().warm_pools, ())
        configured = self.load('[[hosts.warm_pools]]\ntemplate="native"\nsize="recommended"\nready=2\n')
        self.assertEqual(configured.warm_pools, ({"template": "native", "size": "recommended", "ready": 2},))

    def test_invalid_counts_fields_and_duplicate_policies_fail_before_mutation(self):
        base = '[[hosts.warm_pools]]\ntemplate="native"\nsize="small"\n'
        for extra in [base + 'ready=true\n', base + 'ready=-1\n', base + 'ready=33\n',
                      base + 'ready=1\nunrecognized=true\n', (base + 'ready=1\n') * 2]:
            with self.subTest(extra=extra):
                with self.assertRaises(ValueError):
                    self.load(extra)


class WarmPoolInventoryTests(unittest.TestCase):
    @staticmethod
    def parse(vm, config):
        function = next(node for node in ast.parse(INVENTORY_SCRIPT).body
                        if isinstance(node, ast.FunctionDef) and node.name == "pool_metadata")
        namespace = {"json": json, "re": re}
        exec(compile(ast.Module(body=[function], type_ignores=[]), "pool-inventory", "exec"), namespace)
        return namespace["pool_metadata"](vm, config)

    def test_description_claim_overrides_stale_ready_config_without_guest_contact(self):
        payload = {"state": "claimed", "workspace_name": "ebox-owner", "job_id": "b" * 32}
        result = self.parse({"description": PREFIX + json.dumps(payload)}, {"user.enginebox.pool.state": "ready"})
        self.assertEqual(result["warm_pool_state"], "claimed")
        self.assertEqual(result["workspace_name"], "ebox-owner")
        self.assertEqual(result["pool_claim"], payload)

    def test_malformed_reserved_description_is_quarantined(self):
        for payload in ["broken-json", "[]", '{"state":"ready"}', '{"state":"claimed","workspace_name":"bad/name"}']:
            with self.subTest(payload=payload):
                result = self.parse({"description": PREFIX + payload}, {"user.enginebox.pool.state": "ready"})
                self.assertEqual(result["warm_pool_state"], "quarantined")
                self.assertTrue(result["pool_claim"])

    def test_existing_workspace_marker_alone_still_excludes_reuse(self):
        result = self.parse({"description": ""}, {"user.enginebox.pool.state": "ready", "user.enginebox.workspace": "ebox-owner"})
        self.assertEqual(result["warm_pool_state"], "claimed")
        self.assertEqual(result["pool_claim"]["workspace_name"], "ebox-owner")


if __name__ == "__main__":
    unittest.main()
