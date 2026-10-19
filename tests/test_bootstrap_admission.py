"""Atomic fleet commitment admission inside bootstrap's host lock."""
from contextlib import contextmanager
from pathlib import Path
import unittest
from unittest.mock import patch

import bootstrap
from bootstrap_config import normalize, fingerprint
from test_bootstrap import configuration, host_inventory, add_owned_resources


GIB = 1024**3


class BootstrapAdmissionTests(unittest.TestCase):
    def setUp(self):
        self.config = configuration()
        self.config["host"]["admission"] = {"reserve_cpu": 2, "reserve_memory_gib": 4,
                                             "reserve_disk_gib": 10, "max_instances": 2}
        self.observed = host_inventory(self.config)
        self.observed.update(memory_total=64 * GIB, admission_instances=[])

    def vm(self, **updates):
        return {"name": "other", "project": "different", "pool": "existing-pool", "status": "Frozen",
                "cpu": "4", "memory": "4GiB", "disk": "30GiB", "state_disk": "6GiB", "managed": False, **updates}

    def errors(self):
        return bootstrap.admission_errors(self.config, self.observed)

    def test_policy_is_strict_and_does_not_change_guest_fingerprint(self):
        plain = configuration()
        self.assertEqual(fingerprint(plain), fingerprint(normalize(self.config)))
        self.config["host"]["admission"]["max_instances"] = True
        with self.assertRaisesRegex(ValueError, "max_instances"):
            normalize(self.config)
        self.config["host"]["admission"] = {"reserve_cpu": 2}
        with self.assertRaisesRegex(ValueError, "requires"):
            normalize(self.config)

    def test_other_projects_frozen_and_unknown_states_keep_cpu_and_ram(self):
        for status in ["Frozen", "Migrating"]:
            self.observed["admission_instances"] = [self.vm(status=status, cpu="0-13", memory="60GiB")]
            errors = self.errors()
            self.assertTrue(any("CPU" in error for error in errors))
            self.assertTrue(any("RAM" in error for error in errors))

    def test_full_stopped_disk_and_memory_state_commitments_count(self):
        self.observed["admission_instances"] = [self.vm(status="Stopped", disk="155GiB", state_disk="12GiB")]
        errors = self.errors()
        self.assertTrue(any("disk commitments" in error for error in errors))
        self.assertFalse(any("CPU" in error or "RAM" in error for error in errors))

    def test_actual_free_memory_disk_and_slot_limits_remain_enforced(self):
        self.observed["memory_available"] = 7 * GIB
        self.assertTrue(any("RAM" in error for error in self.errors()))
        self.observed["memory_available"] = 32 * GIB
        self.observed["incus_disk_free"] = 47 * GIB  # 30 root + 8 state + 10 reserve
        self.assertTrue(any("disk commitments" in error for error in self.errors()))
        self.observed["incus_disk_free"] = 500 * GIB
        self.observed["admission_instances"] = [self.vm(name=f"clone-{n}", project=self.config["incus"]["project"], managed=True)
                                                  for n in range(2)]
        self.assertTrue(any("instance limit" in error for error in self.errors()))

    def test_same_stopped_base_retry_does_not_double_count_disk_or_slot(self):
        self.observed["pool_resources"]["space"]["total"] = 120 * GIB
        self.observed["admission_instances"] = [self.vm(name=f"clone-{n}", project=self.config["incus"]["project"],
                                                       status="Stopped", managed=True, disk="30GiB", state_disk="6GiB")
                                                  for n in range(2)]
        self.observed["admission_instances"].append(self.vm(name=self.config["workspace"]["name"],
            project=self.config["incus"]["project"], status="Stopped", disk="30GiB", state_disk="8GiB"))
        # 110 GiB committed plus 10 reserve exactly fits; this is an existing
        # slot/base, so retry must not charge another 38 GiB or another slot.
        self.assertEqual(self.errors(), [])

    def test_same_running_base_does_not_double_count_cpu_or_ram(self):
        self.observed["cpu_count"] = 6
        self.observed["memory_total"] = 8 * GIB
        self.observed["memory_available"] = 4 * GIB
        self.observed["admission_instances"] = [self.vm(name=self.config["workspace"]["name"],
            project=self.config["incus"]["project"], status="Running")]
        self.assertEqual(self.errors(), [])

    def test_incomplete_inventory_and_unknown_disk_quota_fail_closed(self):
        del self.observed["admission_instances"]
        self.assertTrue(any("every Incus project" in error for error in self.errors()))
        self.observed["admission_instances"] = [self.vm(disk=None)]
        self.assertTrue(any("disk commitments" in error for error in self.errors()))

    def test_completion_noop_does_not_recharge_resources(self):
        add_owned_resources(self.config, self.observed)
        del self.observed["admission_instances"]
        plan = bootstrap.build_plan(self.config, self.observed)
        self.assertTrue(plan["ok"], plan["errors"])
        self.assertEqual(plan["operations"], [])

    def test_apply_rechecks_allocation_inside_shared_lock_before_each_mutation(self):
        add_owned_resources(self.config, self.observed, prepared=False)
        self.observed["instances"] = []
        locked = False
        reads = 0
        executed = []

        @contextmanager
        def lock(_config):
            nonlocal locked
            locked = True
            try:
                yield
            finally:
                locked = False

        def inspect(_config):
            nonlocal reads
            self.assertTrue(locked, "Commitment inventory must run while holding the host lock")
            reads += 1
            if reads > 1:
                self.observed["admission_instances"].append(self.vm(cpu="14"))
            return self.observed

        def execute(_config, action, _source):
            self.assertTrue(locked)
            executed.append(action)
            self.assertEqual(action, "create_workspace")
            add_owned_resources(self.config, self.observed, prepared=False)
            self.observed["instances"][0]["status"] = "Stopped"
            self.observed["admission_instances"] = [self.vm(name=self.config["workspace"]["name"],
                project=self.config["incus"]["project"], status="Stopped")]

        with patch("bootstrap.os.geteuid", return_value=0), patch("bootstrap.host_lock", side_effect=lock), \
                patch("bootstrap.inventory", side_effect=inspect), patch("bootstrap.execute_operation", side_effect=execute), \
                patch("bootstrap.write_journal"):
            with self.assertRaisesRegex(RuntimeError, "CPU commitments"):
                bootstrap.apply_local(self.config, Path("/unused-assets"))
        self.assertEqual(executed, ["create_workspace"])
        self.assertEqual(reads, 2)
        self.assertFalse(locked)


if __name__ == "__main__":
    unittest.main()
