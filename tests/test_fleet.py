"""Resource admission and authenticated API regressions; never contacts Incus."""
import ast
import json
from contextlib import nullcontext
from dataclasses import replace
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from fleet import Fleet, GIB, Host, capacity, load_hosts
from fleet import INVENTORY_SCRIPT, READINESS_SCRIPT
from fleet_api import handler_for, api_token
import fleet_mcp


def host():
    return Host("primary", "Test host", "test-server", "enginebox", "pool", 4, 8, 10, 30, 8,
                ({"id": "native", "label": "Native", "source": "ebox-base", "snapshot": "ready"},))


def facts():
    return {"cpu_total": 24, "memory_total_bytes": 64 * GIB, "memory_available_bytes": 50 * GIB,
            "disk_free_bytes": 400 * GIB, "pool_total_bytes": 320 * GIB, "pool_used_bytes": 20 * GIB,
            "instances": [{"name": "ebox-base", "project": "enginebox", "status": "Stopped", "cpu": 4,
                           "memory_bytes": 4 * GIB, "disk_bytes": 30 * GIB, "state_disk_bytes": 8 * GIB,
                           "pool": "pool", "managed": False}]}


class CapacityTests(unittest.TestCase):
    def test_unknown_vm_state_keeps_memory_and_cpu_committed(self):
        data = facts()
        data["instances"][0]["status"] = "Migrating"
        self.assertEqual(capacity(host(), data)["allocated_cpu"], 4)

    def test_guest_mapping_rejects_typos_and_does_not_inherit_operator_defaults(self):
        base = 'schema_version=1\n[[hosts]]\nid="one"\ntarget="server"\n'
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fleet.toml"
            path.write_text(base + '[hosts.guest]\npath="/wrong-key"\n')
            with self.assertRaisesRegex(ValueError, "guest accepts"):
                load_hosts(path)
            path.write_text(base + '[hosts.guest]\nuid=true\n')
            with self.assertRaisesRegex(ValueError, "guest.uid"):
                load_hosts(path)
            path.write_text(base)
            with patch.dict(os.environ, {"ENGINEBOX_GUEST_UID": "0", "ENGINEBOX_GUEST": "0"}, clear=True):
                self.assertEqual(load_hosts(path)[0].guest.uid, 1001)

    def test_counts_other_projects_and_frozen_memory(self):
        data = facts()
        data["instances"].append({"name": "other-work", "project": "other", "status": "Frozen", "cpu": 16,
                                  "memory_bytes": 40 * GIB, "disk_bytes": 30 * GIB,
                                  "state_disk_bytes": 42 * GIB, "pool": "pool", "managed": False})
        result = capacity(host(), data)
        self.assertEqual(result["allocated_cpu"], 16)
        self.assertEqual(result["allocated_memory_bytes"], 40 * GIB)
        self.assertEqual(result["max_additional"]["standard"], 1)

    def test_disk_commitments_limit_sparse_clones(self):
        data = facts()
        data["pool_total_bytes"] = 80 * GIB
        self.assertEqual(capacity(host(), data)["max_additional"]["standard"], 0)

    def test_actual_free_memory_limits_budget(self):
        data = facts()
        data["memory_available_bytes"] = 9 * GIB
        self.assertEqual(capacity(host(), data)["max_additional"]["small"], 0)

    def test_stopped_vm_counts_disk_but_not_cpu(self):
        result = capacity(host(), facts())
        self.assertEqual(result["allocated_cpu"], 0)
        self.assertEqual(result["disk_committed_bytes"], 38 * GIB)

    def test_duplicate_hosts_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fleet.toml"
            path.write_text('schema_version=1\n[[hosts]]\nid="one"\ntarget="same"\n[[hosts]]\nid="two"\ntarget="same"\n')
            with self.assertRaisesRegex(ValueError, "unique"):
                load_hosts(path)

    def test_auth_requires_strong_configured_token(self):
        with patch.dict(os.environ, {"ENGINEBOX_API_TOKEN": "short"}, clear=True):
            with self.assertRaises(ValueError):
                api_token()


class InventoryLeaseTests(unittest.TestCase):
    def resolver(self, query):
        # Execute the actual host-script function without /proc/Incus inventory.
        import subprocess
        from urllib.parse import quote
        function = next(node for node in ast.parse(INVENTORY_SCRIPT).body
                        if isinstance(node, ast.FunctionDef) and node.name == "addresses")
        namespace = {"leases": {}, "query": query, "quote": quote, "subprocess": subprocess}
        exec(compile(ast.Module(body=[function], type_ignores=[]), "fleet-inventory", "exec"), namespace)
        return namespace["addresses"]

    def test_shared_bridge_lease_query_and_cache_use_instance_project(self):
        query = Mock(side_effect=[
            [{"hwaddr": "10:66:6a:00:00:01", "address": "10.201.0.10"}],
            [{"hwaddr": "10:66:6a:00:00:01", "address": "10.201.0.20"}],
        ])
        addresses = self.resolver(query)
        vm = {"project": "first", "expanded_config": {"volatile.eth0.hwaddr": "10:66:6a:00:00:01"},
              "expanded_devices": {"eth0": {"type": "nic", "network": "shared-bridge"}}}
        self.assertEqual(addresses(vm), ["10.201.0.10"])
        self.assertEqual(addresses(vm), ["10.201.0.10"])
        self.assertEqual(addresses(vm | {"project": "second"}), ["10.201.0.20"])
        self.assertEqual([call.args[0] for call in query.call_args_list],
                         ["/1.0/networks/shared-bridge/leases?project=first", "/1.0/networks/shared-bridge/leases?project=second"])

    def test_bridged_parent_fallback_uses_project_without_guest_agent(self):
        query = Mock(return_value=[{"hwaddr": "10:66:6a:aa:bb:cc", "address": "10.201.0.30"},
                                   {"hwaddr": "10:66:6a:aa:bb:cc", "address": "fd00::30"}])
        addresses = self.resolver(query)
        vm = {"project": "enginebox-fleet", "expanded_devices": {
            "eth0": {"type": "nic", "nictype": "bridged", "parent": "eboxbr0", "hwaddr": "10:66:6A:AA:BB:CC"}}}
        self.assertEqual(addresses(vm), ["10.201.0.30"])
        query.assert_called_once_with("/1.0/networks/eboxbr0/leases?project=enginebox-fleet")

    def test_nonbridge_parent_is_not_mistaken_for_managed_network(self):
        query = Mock()
        addresses = self.resolver(query)
        vm = {"expanded_devices": {"eth0": {"type": "nic", "nictype": "macvlan", "parent": "eno1"}}}
        self.assertEqual(addresses(vm), [])
        query.assert_not_called()


class FleetTests(unittest.TestCase):
    def setUp(self):
        self.fleet = Fleet([host()])
        self.addCleanup(self.fleet.executor.shutdown)
        lock = patch("fleet.host_operation_lock", side_effect=lambda _host: nullcontext())
        lock.start()
        self.addCleanup(lock.stop)

    def wait_job(self, job):
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            result = self.fleet.job(job["id"])
            if result["status"] in {"failed", "succeeded"}:
                return result
            time.sleep(.01)
        self.fail("Job did not finish.")

    def test_resume_preserves_reserve_and_does_not_start_over_capacity(self):
        data = facts()
        data["instances"][0].update(name="ebox-work", managed=True)
        data["memory_available_bytes"] = 9 * GIB
        with patch("fleet.inventory", return_value=data), patch("fleet.Host.box") as box:
            with self.assertRaisesRegex(ValueError, "Insufficient capacity"):
                self.fleet._action(host(), "ebox-work", "resume")
            box.assert_not_called()

    def test_original_workspace_cannot_be_stopped_by_fleet(self):
        with patch("fleet.inventory", return_value=facts()), patch("fleet.Host.box") as box:
            with self.assertRaisesRegex(ValueError, "fleet-created"):
                self.fleet._action(host(), "ebox-base", "stop")
            box.assert_not_called()

    def test_snapshot_without_clean_marker_cannot_be_cloned(self):
        box = Mock()
        box.run.return_value = b'{"config": {}, "stateful": false}'
        with patch("fleet.inventory", return_value=facts()), patch("fleet.Host.box", return_value=box):
            with self.assertRaisesRegex(ValueError, "before login"):
                self.fleet._create(host(), "ebox-new", "native", "small")
        self.assertEqual(box.run.call_count, 1)  # read-only snapshot query only

    def test_copy_uses_admitted_pool_and_actual_named_root_device(self):
        source_root = {"type": "disk", "path": "/", "pool": "different-source-pool", "size": "30GiB"}
        clone_root = source_root | {"pool": "pool"}
        snapshot = {"config": {"user.enginebox.clean-base": "true"}, "stateful": False,
                    "expanded_devices": {"system-disk": source_root}}
        clone = {"devices": {"system-disk": clone_root}, "expanded_devices": {"system-disk": clone_root}}
        box = Mock()
        box.run.side_effect = [json.dumps(snapshot).encode(), b"", json.dumps(clone).encode(), b"", b""]
        with patch("fleet.inventory", return_value=facts()), patch("fleet.Host.box", return_value=box), \
                patch.object(self.fleet, "_wait_ready", return_value={"ready": True}):
            result = self.fleet._create(host(), "ebox-new", "native", "standard")
        copy = box.run.call_args_list[1].args
        self.assertEqual(copy[copy.index("--storage") + 1], "pool")
        self.assertIn("user.enginebox.fleet=true", copy)
        self.assertIn("user.enginebox.clean-base=false", copy)
        self.assertEqual(box.run.call_args_list[3].args[:5], ("config", "device", "set", "ebox-new", "system-disk"))
        self.assertTrue(result["readiness"]["ready"])

    def test_template_disk_cannot_be_implicitly_shrunk(self):
        snapshot = {"config": {"user.enginebox.clean-base": "true"}, "stateful": False,
                    "expanded_devices": {"root": {"type": "disk", "path": "/", "pool": "pool", "size": "30GiB"}}}
        box = Mock()
        box.run.return_value = json.dumps(snapshot).encode()
        with patch("fleet.inventory", return_value=facts()), patch("fleet.Host.box", return_value=box):
            with self.assertRaisesRegex(ValueError, "shrinking"):
                self.fleet._create(replace(host(), disk_gib=16), "ebox-new", "native", "small")
        self.assertEqual(box.run.call_count, 1)

    def test_wrong_postcopy_pool_never_starts_clone(self):
        root = {"type": "disk", "path": "/", "pool": "wrong-pool", "size": "30GiB"}
        snapshot = {"config": {"user.enginebox.clean-base": "true"}, "stateful": False,
                    "expanded_devices": {"root": root}}
        box = Mock()
        box.run.side_effect = [json.dumps(snapshot).encode(), b"", json.dumps({"expanded_devices": {"root": root}}).encode()]
        with patch("fleet.inventory", return_value=facts()), patch("fleet.Host.box", return_value=box):
            with self.assertRaisesRegex(RuntimeError, "admitted pool"):
                self.fleet._create(host(), "ebox-new", "native", "small")
        self.assertFalse(any(call.args[0] == "start" for call in box.run.call_args_list))

    def test_actions_refuse_vm_moved_to_another_pool(self):
        data = facts()
        data["instances"][0].update(name="ebox-work", managed=True, pool="different-pool")
        with patch("fleet.inventory", return_value=data), patch("fleet.Host.box") as box:
            with self.assertRaisesRegex(ValueError, "admission pool"):
                self.fleet._action(host(), "ebox-work", "hibernate")
            box.assert_not_called()

    def test_readiness_requires_structured_probe_and_passes_exact_guest_identity(self):
        box = Mock()
        box.guest = replace(host().guest, uid=2000, gid=2001, user="builder", home="/home/builder", workspace="/work")
        box.execute.return_value = b'{"ready":true,"ipv4":["10.0.0.2"],"dns":true}'
        result = self.fleet._wait_ready(box, "ebox-work")
        self.assertTrue(result["ready"])
        command = box.execute.call_args.args[1]
        self.assertEqual(command[:3], ["python3", "-c", READINESS_SCRIPT])
        expected = json.loads(command[3])
        self.assertEqual((expected["uid"], expected["gid"], expected["workspace"]), (2000, 2001, "/work"))
        box.execute.return_value = b'{}'
        with patch("fleet.time.monotonic", side_effect=[0, 0, 181]), patch("fleet.time.sleep"):
            with self.assertRaisesRegex(RuntimeError, "identity, manifest, network"):
                self.fleet._wait_ready(box, "ebox-work")

    def test_failed_job_persistence_does_not_leak_queue_slot(self):
        with patch.object(self.fleet, "_save", side_effect=OSError("disk full")):
            with self.assertRaisesRegex(OSError, "disk full"):
                self.fleet.submit("primary", "stop", name="ebox-work")
        acquired = [self.fleet.pending.acquire(blocking=False) for _ in range(32)]
        self.assertTrue(all(acquired))
        for success in acquired:
            if success:
                self.fleet.pending.release()
        self.assertEqual(self.fleet.jobs, {})

    def test_jobs_on_a_host_do_not_overlap(self):
        running, maximum = 0, 0
        lock = threading.Lock()
        def action(*args):
            nonlocal running, maximum
            with lock:
                running += 1
                maximum = max(maximum, running)
            time.sleep(.04)
            with lock:
                running -= 1
            return {"status": "Stopped"}
        with patch.object(self.fleet, "_action", side_effect=action):
            jobs = [self.fleet.submit("primary", "stop", name=f"ebox-test-{i}") for i in range(3)]
            self.assertTrue(all(self.wait_job(job)["status"] == "succeeded" for job in jobs))
        self.assertEqual(maximum, 1)

    def test_restart_never_reports_inflight_job_as_completed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / ("1" * 32 + ".json")).write_text(json.dumps({"id": "1" * 32, "status": "running"}))
            fleet = Fleet([host()], state_dir=path)
            self.addCleanup(fleet.executor.shutdown)
            result = fleet.job("1" * 32)
            self.assertEqual(result["status"], "failed")
            self.assertIn("inspect", result["error"])
            self.assertEqual(json.loads((path / ("1" * 32 + ".json")).read_text())["status"], "failed")

    def test_persisted_job_cannot_escape_state_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "bad.json").write_text(json.dumps({"id": "../outside", "status": "running"}))
            with self.assertRaisesRegex(ValueError, "Invalid fleet job"):
                Fleet([host()], state_dir=path)


class APITests(unittest.TestCase):
    def setUp(self):
        self.fleet = Mock()
        self.fleet.summary.return_value = {"hosts": []}
        self.fleet.submit.return_value = {"id": "1" * 32, "status": "queued"}
        self.token = "a" * 48
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(self.fleet, self.token))
        thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.base = f"http://127.0.0.1:{self.server.server_port}"

    def request(self, path, body=None, auth=True, origin=None):
        headers = {"Content-Type": "application/json"}
        if auth:
            headers["Authorization"] = "Bearer " + self.token
        if origin:
            headers["Origin"] = origin
        req = Request(self.base + path, headers=headers, data=None if body is None else json.dumps(body).encode())
        try:
            with urlopen(req, timeout=2) as response:
                return response.status, json.load(response)
        except HTTPError as response:
            with response:
                return response.code, json.load(response)

    def test_unauthenticated_request_cannot_inspect_or_mutate(self):
        self.assertEqual(self.request("/v1/fleet", auth=False)[0], 401)
        self.assertEqual(self.request("/v1/hosts/primary/instances", {"name": "ebox-one"}, auth=False)[0], 401)
        self.fleet.summary.assert_not_called()
        self.fleet.submit.assert_not_called()

    def test_browser_origin_rejected_even_with_valid_token(self):
        self.assertEqual(self.request("/v1/fleet", origin="https://untrusted.example")[0], 403)
        self.fleet.summary.assert_not_called()

    def test_create_is_async_and_unknown_fields_rejected(self):
        body = {"name": "ebox-one", "template": "native", "size": "small"}
        status, result = self.request("/v1/hosts/primary/instances", body)
        self.assertEqual(status, 202)
        self.assertEqual(result["job"]["status"], "queued")
        self.assertEqual(self.request("/v1/hosts/primary/instances", body | {"command": "arbitrary"})[0], 400)
        self.fleet.submit.assert_called_once()

    def test_health_contains_no_fleet_data(self):
        self.assertEqual(self.request("/healthz", auth=False), (200, {"ok": True}))


class MCPRedirectTests(unittest.TestCase):
    def test_mcp_never_forwards_bearer_to_redirect_target(self):
        received = []
        class Target(BaseHTTPRequestHandler):
            def do_GET(self):
                received.append(self.headers.get("Authorization"))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'{}')
            def log_message(self, *_args): pass
        target = ThreadingHTTPServer(("127.0.0.1", 0), Target)
        class Redirect(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(302)
                self.send_header("Location", f"http://127.0.0.1:{target.server_port}/secret-target")
                self.send_header("Content-Length", "0")
                self.end_headers()
            def log_message(self, *_args): pass
        source = ThreadingHTTPServer(("127.0.0.1", 0), Redirect)
        for server in [target, source]:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            self.addCleanup(server.server_close)
            self.addCleanup(server.shutdown)
        with patch.dict(os.environ, {"ENGINEBOX_API_URL": f"http://127.0.0.1:{source.server_port}",
                                     "ENGINEBOX_API_TOKEN": "x" * 48}, clear=True):
            with self.assertRaisesRegex(RuntimeError, "302"):
                fleet_mcp.request("/v1/fleet")
        self.assertEqual(received, [])


if __name__ == "__main__":
    unittest.main()
