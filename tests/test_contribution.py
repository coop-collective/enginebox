"""Aggregate personal-node contribution ceilings across every provisioning path."""
from contextlib import nullcontext
from dataclasses import replace
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import bootstrap
from bootstrap_config import normalize
from fleet import Fleet, GIB, capacity, load_hosts, resource_fits
from templates import bootstrap_configuration, validate_manifest
from test_bootstrap import configuration, host_inventory
from test_fleet import facts, host
from test_templates import manifest
from warm_pool import generation


class ContributionTests(unittest.TestCase):
    def limited_host(self, **overrides):
        return replace(host(), contribution_cpu=4, contribution_memory_gib=8, **overrides)

    def running(self, **overrides):
        return {"name": "ebox-work", "project": "enginebox", "pool": "pool", "status": "Running",
                "cpu": 4, "memory_bytes": 4 * GIB, "disk_bytes": 30 * GIB,
                "state_disk_bytes": 6 * GIB, "managed": True, **overrides}

    def fleet(self, selected):
        fleet = Fleet([selected])
        self.addCleanup(fleet.executor.shutdown)
        return fleet

    def test_config_requires_both_strict_positive_caps_with_upper_bounds(self):
        base = 'schema_version=1\n[[hosts]]\nid="one"\ntarget="server"\n'
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fleet.toml"
            for suffix in ['contribution_cpu=4\n', 'contribution_memory_gib=8\n',
                           'contribution_cpu=true\ncontribution_memory_gib=8\n',
                           'contribution_cpu=4.0\ncontribution_memory_gib=8\n',
                           'contribution_cpu=0\ncontribution_memory_gib=8\n',
                           'contribution_cpu=4097\ncontribution_memory_gib=8\n',
                           'contribution_cpu=4\ncontribution_memory_gib=16385\n']:
                path.write_text(base + suffix)
                with self.subTest(suffix=suffix), self.assertRaisesRegex(ValueError, "contribution"):
                    load_hosts(path)
            path.write_text(base + 'contribution_cpu=4\ncontribution_memory_gib=8\n')
            loaded = load_hosts(path)[0]
            self.assertEqual((loaded.contribution_cpu, loaded.contribution_memory_gib), (4, 8))
            path.write_text(base)
            self.assertIsNone(load_hosts(path)[0].contribution_cpu)

    def test_cpu_and_memory_caps_limit_real_resource_admission(self):
        selected, data = self.limited_host(), facts()
        room = capacity(selected, data)
        self.assertEqual(room["cpu_available"], 4)
        self.assertEqual(room["memory_budget_available_bytes"], 8 * GIB)
        for resources in [{"cpu": 5, "memory_gib": 4, "disk_gib": 30},
                          {"cpu": 4, "memory_gib": 9, "disk_gib": 30}]:
            self.assertFalse(resource_fits(selected, data, resources))
        self.assertTrue(resource_fits(selected, data, {"cpu": 4, "memory_gib": 8, "disk_gib": 30}))

    def test_frozen_unknown_and_owned_template_allocations_consume_contribution(self):
        for state in ["Running", "Frozen", "Migrating", "Starting"]:
            for ownership in [{"managed": True}, {"managed": False, "enginebox_owned": True}]:
                data = facts()
                data["instances"].append(self.running(status=state, project="other", **ownership))
                room = capacity(self.limited_host(), data)
                self.assertEqual(room["cpu_available"], 0)
                self.assertEqual(room["contribution_allocated_cpu"], 4)
        data["instances"][-1]["status"] = "Stopped"
        self.assertEqual(capacity(self.limited_host(), data)["cpu_available"], 4)

    def test_unrelated_work_keeps_physical_reserves_without_consuming_contribution(self):
        data = facts()
        data["instances"].append(self.running(cpu=6, managed=False))
        room = capacity(self.limited_host(), data)
        self.assertEqual(room["contribution_allocated_cpu"], 0)
        self.assertEqual(room["cpu_available"], 4)
        data["instances"][-1]["cpu"] = 19
        self.assertEqual(capacity(self.limited_host(), data)["cpu_available"], 1)
        data["memory_available_bytes"] = 9 * GIB
        self.assertEqual(capacity(self.limited_host(), data)["memory_budget_available_bytes"], GIB)

    def test_large_caps_cannot_expand_physical_capacity_or_remove_reserves(self):
        capped = replace(host(), contribution_cpu=4096, contribution_memory_gib=16384)
        plain = capacity(host(), facts())
        limited = capacity(capped, facts())
        for field in ["cpu_available", "memory_budget_available_bytes", "max_additional"]:
            self.assertEqual(plain[field], limited[field])

    def test_create_and_stopped_resume_reject_before_transport(self):
        selected = self.limited_host()
        fleet = self.fleet(selected)
        with patch("fleet.inventory", return_value=facts()), patch("fleet.Host.box") as box:
            with self.assertRaisesRegex(ValueError, "Insufficient"):
                fleet._create(selected, "ebox-large", "native", "large")
            box.assert_not_called()
        data = facts()
        data["instances"].append(self.running(name="ebox-stopped", status="Stopped", cpu=8))
        with patch("fleet.inventory", return_value=data), patch("fleet.Host.box") as box:
            with self.assertRaisesRegex(ValueError, "Insufficient"):
                fleet._action(selected, "ebox-stopped", "resume")
            box.assert_not_called()

    def test_warm_refill_uses_real_contribution_admission(self):
        selected = self.limited_host(warm_pools=({"template": "native", "size": "small", "ready": 1},))
        fleet = self.fleet(selected)
        data = facts()
        data["instances"].append(self.running(status="Frozen"))
        self.assertTrue(resource_fits(host(), data, {"cpu": 2, "memory_gib": 2, "disk_gib": 30}))
        with patch("fleet.inventory", return_value=data), \
             patch("fleet.host_operation_lock", return_value=nullcontext()), \
             patch.object(fleet, "template_snapshot", return_value={"config": {"user.enginebox.clean-base": "true"}}), \
             patch.object(fleet, "_create") as create:
            self.assertFalse(fleet.warm.reconcile_once(selected))
        create.assert_not_called()
        self.assertIn("capacity", fleet.warm.errors[(selected.id, "native", "small")])

    def test_warm_claim_at_full_cap_reuses_reserved_budget(self):
        selected = replace(host(), contribution_cpu=2, contribution_memory_gib=2)
        fleet = self.fleet(selected)
        snapshot = {"config": {"user.enginebox.clean-base": "true"}}
        template = fleet.template(selected, "native")
        resources = fleet.resources(selected, template, "small")
        expected = generation(selected, template, resources, snapshot)
        data = facts()
        data["instances"].append(self.running(name="ebox-warm-one", status="Frozen", cpu=2, memory_bytes=2 * GIB,
                                             warm_pool_state="ready", template="native", pool_size="small",
                                             pool_generation=expected))
        self.assertEqual(capacity(selected, data)["cpu_available"], 0)
        with patch("fleet.inventory", return_value=data), patch("fleet.Host.box", return_value=Mock()), \
             patch.object(fleet, "template_snapshot", return_value=snapshot), \
             patch.object(fleet, "_wait_ready", return_value={"ready": True}), \
             patch.object(fleet, "_create") as create:
            with fleet.locks[selected.id]:
                result = fleet.warm.allocate(selected, "ebox-assigned", "native", "small", job_id="a" * 32)
        self.assertEqual(result["startup"], "warm")
        create.assert_not_called()

    def test_template_bootstrap_propagates_and_enforces_caps(self):
        selected = self.limited_host()
        config = bootstrap_configuration(selected, validate_manifest(manifest()), "bridge")
        policy = config["host"]["admission"]
        self.assertEqual(policy["contribution_cpu"], 4)
        self.assertEqual(policy["contribution_memory_gib"], 8)
        observed = host_inventory(config)
        observed.update(memory_total=64 * GIB, admission_instances=[{
            "name": "ebox-preparing", "project": selected.project, "status": "Running", "pool": selected.pool,
            "cpu": "2", "memory": "6GiB", "disk": "30GiB", "state_disk": "8GiB",
            "managed": False, "enginebox_owned": True}])
        errors = bootstrap.admission_errors(config, observed)
        self.assertTrue(any("CPU" in error and "contribution" in error for error in errors))
        self.assertTrue(any("RAM" in error and "contribution" in error for error in errors))
        observed["admission_instances"][0]["enginebox_owned"] = False
        self.assertFalse(any("contribution" in error for error in bootstrap.admission_errors(config, observed)))

    def test_bootstrap_rejects_partial_boolean_and_oversized_contribution_policy(self):
        config = bootstrap_configuration(self.limited_host(), validate_manifest(manifest()), "bridge")
        for change in [{"contribution_cpu": True}, {"contribution_memory_gib": 0}, {"contribution_cpu": 4097}]:
            policy = config["host"]["admission"] | change
            with self.subTest(change=change), self.assertRaisesRegex(ValueError, "contribution"):
                normalize(config | {"host": config["host"] | {"admission": policy}})
        del config["host"]["admission"]["contribution_cpu"]
        with self.assertRaisesRegex(ValueError, "together"):
            normalize(config)


if __name__ == "__main__":
    unittest.main()
