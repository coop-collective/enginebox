"""Authenticated template/allocation integration with mocked Incus, never a host."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, contextmanager
from copy import deepcopy
from dataclasses import replace
from http.server import ThreadingHTTPServer
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from fleet import Fleet, GIB, Host
from fleet_api import handler_for
from templates import Registry
from warm_pool import PREFIX, generation


def host():
    return Host("primary", "Test", "not-a-real-host", "enginebox", "pool", 2, 4, 10, 30, 16,
                ({"id": "native", "label": "Native", "source": "ebox-base", "snapshot": "prepared",
                  "minimum": {"cpu": 2, "memory_gib": 2, "disk_gib": 30},
                  "recommended": {"cpu": 4, "memory_gib": 4, "disk_gib": 40}},))


def manifest(revision=1):
    return {"schema_version": 1, "id": "native", "revision": revision, "label": "Native",
            "minimum": {"cpu": 2, "memory_gib": 2, "disk_gib": 30},
            "recommended": {"cpu": 4, "memory_gib": 4, "disk_gib": 40}}


class FleetAllocationHTTPTests(unittest.TestCase):
    TOKEN = "test-token-for-local-unit-server-only-123456789"

    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.host = host()
        self.fleet = Fleet([self.host])
        self.addCleanup(self.fleet.executor.shutdown, wait=True)
        self.box = Mock()
        self.box.guest = self.host.guest
        self.box.run.side_effect = self.run_incus
        self.box.lifecycle.side_effect = self.lifecycle
        self.box.execute.return_value = b'{"ready":true,"identity":true,"dns":true}'
        self.snapshot = {"name": "ebox-base/prepared", "created_at": "2026-10-19T00:00:00Z",
                         "config": {"user.enginebox.clean-base": "true"}, "stateful": False,
                         "expanded_devices": {"root": {"type": "disk", "path": "/", "pool": "source-pool", "size": "30GiB"}}}
        self.vms = {"ebox-base": {"name": "ebox-base", "project": "enginebox", "status": "Stopped", "cpu": 4,
                                  "memory_bytes": 4 * GIB, "disk_bytes": 30 * GIB, "state_disk_bytes": 6 * GIB,
                                  "pool": "pool", "managed": False}}
        self.calls = []
        self.host_lock_depth = 0
        self.stack.enter_context(patch("fleet.Host.box", return_value=self.box))
        self.stack.enter_context(patch("fleet.inventory", side_effect=self.inventory))
        self.stack.enter_context(patch("fleet.host_operation_lock", side_effect=self.host_lock))
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(self.fleet, self.TOKEN))
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True)
        self.thread.start()
        self.addCleanup(self.close_server)
        self.origin = f"http://127.0.0.1:{self.server.server_port}"

    def close_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    @contextmanager
    def host_lock(self, _host):
        self.host_lock_depth += 1
        try:
            yield
        finally:
            self.host_lock_depth -= 1

    def inventory(self, _host):
        return {"cpu_total": 32, "memory_total_bytes": 128 * GIB, "memory_available_bytes": 100 * GIB,
                "disk_free_bytes": 1024 * GIB, "pool_total_bytes": 1024 * GIB, "pool_used_bytes": 100 * GIB,
                "instances": deepcopy(list(self.vms.values()))}

    def run_incus(self, *argv, **kwargs):
        self.calls.append(argv)
        if argv[0] == "query":
            if "/snapshots/" in argv[1]:
                return json.dumps(self.snapshot).encode()
            name = argv[1].split("/instances/", 1)[1].split("?", 1)[0]
            return json.dumps({"name": name, "devices": {"root": {"type": "disk", "path": "/", "pool": "pool", "size": "30GiB"}}}).encode()
        if argv[0] == "copy":
            name = argv[2]
            self.vms[name] = {"name": name, "project": "enginebox", "status": "Stopped", "pool": "pool", "managed": True}
        elif argv[0] == "start":
            self.vms[argv[1]]["status"] = "Running"
        elif argv[:2] == ("config", "set"):
            self.assertTrue(self.fleet.locks[self.host.id].locked())
            self.assertEqual(self.host_lock_depth, 1)
            vm = self.vms[argv[2]]
            for argument in argv[3:]:
                if argument.startswith("description=" + PREFIX):
                    payload = json.loads(argument.split("=", 1)[1][len(PREFIX):])
                    vm.update(pool_claim=payload, warm_pool_state=payload["state"], workspace_name=payload["workspace_name"])
        elif argv[:2] != ("config", "device"):
            raise AssertionError(f"Unexpected Incus command: {argv}")
        return b""

    def lifecycle(self, name, action):
        self.assertEqual(action, "resume")
        self.assertEqual(self.vms[name]["pool_claim"]["state"], "claimed")
        self.vms[name]["status"] = "Running"
        return {"name": name, "status": "Running"}

    def add_warm_member(self, resources):
        template = self.fleet.template(self.host, "native")
        self.vms["ebox-warm-ready"] = {"name": "ebox-warm-ready", "project": "enginebox", "status": "Frozen",
            "pool": "pool", "managed": True, "template": "native", "warm_pool_state": "ready", "pool_claim": None,
            "pool_generation": generation(self.host, template, resources, self.snapshot),
            "cpu": resources["cpu"], "memory_bytes": resources["memory_gib"] * GIB,
            "disk_bytes": resources["disk_gib"] * GIB, "state_disk_bytes": (resources["memory_gib"] + 2) * GIB}

    def request(self, path, body=None, *, authorized=True):
        headers = {"Content-Type": "application/json"}
        if authorized:
            headers["Authorization"] = "Bearer " + self.TOKEN
        request = Request(self.origin + path, data=json.dumps(body).encode() if body is not None else None, headers=headers)
        try:
            response = urlopen(request, timeout=3)
        except HTTPError as error:
            response = error
        with response:
            return response.status, json.loads(response.read())

    def finish(self, job):
        self.fleet.executor.shutdown(wait=True)
        status, response = self.request("/v1/jobs/" + job["id"])
        self.assertEqual(status, 200)
        self.assertEqual(response["job"]["status"], "succeeded", response)
        return response["job"]["result"]

    def test_authenticated_custom_allocation_runs_real_queue_and_warm_claim(self):
        custom = {"cpu": 6, "memory_gib": 8, "disk_gib": 60}
        self.add_warm_member(custom)
        body = {"name": "ebox-owner", "template": "native", "resources": custom}
        status, _ = self.request("/v1/hosts/primary/allocations", body, authorized=False)
        self.assertEqual(status, 401)
        self.assertEqual(self.fleet.jobs, {})
        with patch("fleet.resource_fits", side_effect=AssertionError("Warm assignment must not require new budget")):
            status, response = self.request("/v1/hosts/primary/allocations", body)
            self.assertEqual(status, 202)
            result = self.finish(response["job"])
        self.assertEqual((result["startup"], result["name"], result["workspace_name"]),
                         ("warm", "ebox-warm-ready", "ebox-owner"))
        self.assertFalse(any(call[0] == "copy" for call in self.calls))
        self.assertEqual(self.vms["ebox-warm-ready"]["pool_claim"]["job_id"], response["job"]["id"])

    def test_custom_cold_allocation_propagates_cpu_memory_and_disk(self):
        custom = {"cpu": 6, "memory_gib": 8, "disk_gib": 60}
        status, response = self.request("/v1/hosts/primary/allocations",
                                        {"name": "ebox-custom", "template": "native", "resources": custom})
        self.assertEqual(status, 202)
        result = self.finish(response["job"])
        self.assertEqual((result["startup"], result["name"]), ("cold", "ebox-custom"))
        copy = next(call for call in self.calls if call[0] == "copy")
        self.assertIn("limits.cpu=6", copy)
        self.assertIn("limits.memory=8GiB", copy)
        self.assertEqual(copy[copy.index("--storage") + 1], "pool")
        device = next(call for call in self.calls if call[:2] == ("config", "device"))
        self.assertIn("size=60GiB", device)
        self.assertIn("size.state=10GiB", device)

    def test_concurrent_http_requests_cannot_claim_the_same_member(self):
        resources = {"cpu": 2, "memory_gib": 2, "disk_gib": 30}
        self.add_warm_member(resources)
        barrier = threading.Barrier(2)
        def request(name):
            barrier.wait(timeout=3)
            return self.request("/v1/hosts/primary/allocations", {"name": name, "template": "native", "resources": resources})
        with ThreadPoolExecutor(max_workers=2) as requests:
            responses = list(requests.map(request, ["ebox-first", "ebox-second"]))
        self.assertEqual([status for status, _ in responses], [202, 202])
        results = [self.finish(response["job"]) for _, response in responses]
        self.assertEqual(sorted(result["startup"] for result in results), ["cold", "warm"])
        self.assertEqual(len({result["name"] for result in results}), 2)
        descriptions = [call for call in self.calls if call[:3] == ("config", "set", "ebox-warm-ready")
                        and "--property" in call]
        self.assertEqual(len(descriptions), 1)

    def test_invalid_custom_requests_fail_before_queue_or_host_mutation(self):
        base = {"name": "ebox-invalid", "template": "native"}
        for extra in [{"resources": {"cpu": 1, "memory_gib": 2, "disk_gib": 30}},
                      {"resources": {"cpu": True, "memory_gib": 4, "disk_gib": 30}},
                      {"resources": {"cpu": 4}},
                      {"size": "small", "resources": {"cpu": 4, "memory_gib": 4, "disk_gib": 30}}]:
            with self.subTest(extra=extra):
                status, _ = self.request("/v1/hosts/primary/allocations", base | extra)
                self.assertEqual(status, 400)
        self.assertEqual(self.fleet.jobs, {})
        self.assertEqual(self.calls, [])

    def test_unassigned_reserves_cannot_be_changed_with_workspace_actions(self):
        self.add_warm_member({"cpu": 2, "memory_gib": 2, "disk_gib": 30})
        status, response = self.request("/v1/hosts/primary/instances/ebox-warm-ready/actions", {"action": "resume"})
        self.assertEqual(status, 202)
        self.fleet.executor.shutdown(wait=True)
        result = self.fleet.job(response["job"]["id"])
        self.assertEqual(result["status"], "failed")
        self.assertIn("Unassigned warm reserves", result["error"])
        self.box.lifecycle.assert_not_called()


class FleetRegistryBindingTests(unittest.TestCase):
    def test_persisted_deployment_requires_current_host_binding_and_revision(self):
        with tempfile.TemporaryDirectory() as directory:
            registry = Registry(directory)
            current_host = host()
            registry.register(manifest())
            with patch("templates.install_template", return_value={"source": "ebox-template", "snapshot": "prepared", "network": "bridge"}):
                registry.install(current_host, "native")
            # A fresh controller must read the durable receipt, not process-local state.
            current = Fleet([current_host], registry=Registry(directory))
            self.addCleanup(current.executor.shutdown, wait=True)
            self.assertEqual(current.template(current_host, "native")["source"], "ebox-template")
            for changed_host in [replace(current_host, target="different-host"),
                                 replace(current_host, pool="different-pool"),
                                 replace(current_host, guest=replace(current_host.guest, uid=2001)),
                                 replace(current_host, network="different-bridge")]:
                with self.subTest(changed_host=changed_host):
                    changed = Fleet([changed_host], registry=Registry(directory))
                    self.addCleanup(changed.executor.shutdown, wait=True)
                    self.assertEqual(changed.templates_for(changed_host), [])
                    self.assertFalse(changed.catalog()["templates"][0]["deployments"][0]["in_sync"])
                    with self.assertRaisesRegex(ValueError, "current revision"):
                        changed.submit("primary", "allocate", name="ebox-wrong-host", template="native")
            registry.register(manifest(revision=2))
            self.assertEqual(current.templates_for(current_host), [])
            self.assertFalse(current.catalog()["templates"][0]["deployments"][0]["in_sync"])

    def test_persistent_fleet_creates_registry_beside_jobs(self):
        with tempfile.TemporaryDirectory() as directory:
            fleet = Fleet([host()], state_dir=Path(directory) / "jobs")
            self.addCleanup(fleet.executor.shutdown, wait=True)
            fleet.registry.register(manifest())
            reopened = Fleet([host()], state_dir=Path(directory) / "jobs")
            self.addCleanup(reopened.executor.shutdown, wait=True)
            self.assertEqual(reopened.registry.get("native")["revision"], 1)
            self.assertEqual(reopened.registry.root.parent, Path(directory).resolve())


if __name__ == "__main__":
    unittest.main()
