"""Read-only portable inventory, fail-closed allocation and platform boundary tests."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "runtime"))
import host_capabilities as host


class PortableHostTests(unittest.TestCase):
    def test_personal_budget_preserves_cpu_memory_and_disk(self):
        report = host.allocation(16, 32 * host.GIB, 200 * host.GIB)
        self.assertEqual(report["requested"], {"cpus": 8, "memory_bytes": 16 * host.GIB,
                                              "disk_bytes": 100 * host.GIB})
        self.assertFalse(report["enforced"])
        self.assertEqual(report["status"], "suggested")

    def test_low_resources_and_missing_observation_fail_closed(self):
        for cpu, memory, disk in [(2, 16, 100), (8, 4, 100), (8, 16, 20)]:
            with self.subTest(cpu=cpu, memory=memory, disk=disk):
                self.assertEqual(host.allocation(cpu, memory * host.GIB, disk * host.GIB)["status"], "rejected")
        self.assertEqual(host.allocation(8, None, 100 * host.GIB)["status"], "unknown")

    def test_custom_allocation_cannot_consume_owner_reserve(self):
        for overrides in [{"requested_cpu": 7}, {"requested_memory_gib": 12}, {"requested_disk_gib": 90}]:
            self.assertEqual(host.allocation(8, 16 * host.GIB, 100 * host.GIB, **overrides)["status"], "rejected")
        self.assertEqual(host.allocation(8, 16 * host.GIB, 100 * host.GIB, 2, 4, 25)["status"], "suggested")

    def test_unknown_docker_capacity_not_added_to_physical_capacity(self):
        with patch.object(host.platform, "system", return_value="Darwin"), \
             patch.object(host.platform, "machine", return_value="arm64"), \
             patch.object(host.os, "cpu_count", return_value=8), \
             patch.object(host, "memory_bytes", return_value=16 * host.GIB), \
             patch.object(host, "graphics", return_value={"devices": []}), \
             patch.object(host, "docker_inventory", return_value={"status": "observed", "cpus": 8, "memory_bytes": 8 * host.GIB}):
            report = host.inspect_host(Path.cwd(), docker=True)
        self.assertEqual(report["machine"]["cpus"], 8)
        self.assertEqual(report["allocation"]["maximum"]["cpus"], 4)
        self.assertEqual(report["docker"]["allocation"]["maximum"]["memory_bytes"], 4 * host.GIB)
        self.assertEqual(report["backends"]["incus_linux_vm"]["status"], "not_eligible")
        for name in ["docker_workspace", "native_windows", "native_macos"]:
            self.assertFalse(report["backends"][name]["enrollment_enabled"])

    def test_docker_opt_in_is_required(self):
        with patch.object(host, "probe") as command:
            self.assertEqual(host.docker_inventory(False)["status"], "not_requested")
            command.assert_not_called()

    def test_remote_docker_context_is_not_contacted_or_disclosed(self):
        for endpoint in ["ssh://private-host", "tcp://private-host:2376", "tcp://127.0.0.1:2375",
                         "npipe:////private-host/pipe/docker_engine"]:
            with patch.dict(os.environ, {"DOCKER_HOST": endpoint}, clear=True), patch.object(host, "probe") as command:
                report = host.docker_inventory(True)
                command.assert_not_called()
            self.assertEqual(report["status"], "remote_context_refused")
            self.assertNotIn(endpoint, json.dumps(report))

    def test_docker_context_override_beats_docker_host(self):
        responses = [json.dumps("unix:///private/local.sock"), json.dumps({"cpus": 4, "memory_bytes": 8 * host.GIB,
                     "os": "linux", "architecture": "aarch64", "server_version": "29.0", "security_options": ["name=rootless"]})]
        with patch.dict(os.environ, {"DOCKER_HOST": "ssh://private-host", "DOCKER_CONTEXT": "desktop-linux"}, clear=True), \
             patch.object(host, "probe", side_effect=responses) as command:
            report = host.docker_inventory(True)
        self.assertEqual(command.call_count, 2)
        self.assertEqual(report["cpus"], 4)
        self.assertTrue(report["rootless"])
        self.assertNotIn("private", json.dumps(report))

    def test_docker_context_must_resolve_and_info_must_parse(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(host, "probe", return_value="not JSON"):
            self.assertEqual(host.docker_inventory(True)["status"], "unavailable")
        with patch.dict(os.environ, {"DOCKER_HOST": "npipe:////./pipe/docker_engine"}, clear=True), \
             patch.object(host, "probe", return_value="not JSON"):
            self.assertEqual(host.docker_inventory(True)["status"], "unavailable")

    def test_mac_gpu_detection_does_not_claim_passthrough_or_vram(self):
        with patch.object(host, "json_probe", return_value={"SPDisplaysDataType": [{"sppci_model": "Apple M4"}]}):
            report = host.graphics("Darwin")
        self.assertEqual(report["devices"][0]["name"], "Apple M4")
        self.assertIsNone(report["devices"][0]["memory_bytes"])
        self.assertFalse(report["schedulable"])
        self.assertFalse(report["passthrough_tested"])

    def test_windows_graphics_single_device_and_missing_probe(self):
        with patch.object(host, "json_probe", return_value={"Name": "NVIDIA RTX"}):
            self.assertEqual(len(host.graphics("Windows")["devices"]), 1)
        with patch.object(host, "json_probe", return_value=None):
            self.assertEqual(host.graphics("Windows")["status"], "unknown")

    def test_probe_timeout_and_failure_never_leak_stderr(self):
        with patch.object(host.subprocess, "run", side_effect=subprocess.TimeoutExpired("probe", 15)):
            self.assertIsNone(host.probe(["probe"]))
        response = subprocess.CompletedProcess(["probe"], 1, "private stdout", "private stderr")
        with patch.object(host.subprocess, "run", return_value=response) as command:
            self.assertIsNone(host.probe(["probe"]))
        self.assertEqual(command.call_args.kwargs["timeout"], 15)
        self.assertNotIn("shell", command.call_args.kwargs)

    def test_non_finite_overflow_and_negative_cli_budgets_rejected(self):
        for value in ["nan", "inf", "-1", "0", "1e308"]:
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                host.positive_number(value)


if __name__ == "__main__":
    unittest.main()
