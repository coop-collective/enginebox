"""Controller template desired state converges across existing and newly added nodes."""
from dataclasses import replace
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import Mock

from fleet import Host
from template_sync import TemplateSync
from templates import host_binding_digest


class TemplateSyncTests(unittest.TestCase):
    def setUp(self):
        first = Host("one", "First", "host-one", "enginebox", "pool", 2, 4, 10, 30, 8, ())
        second = replace(first, id="two", label="Second", target="host-two")
        self.record = {"id": "native", "revision": 1, "manifest_digest": "digest-one", "deployments": []}
        registry = Mock()
        registry.list.side_effect = lambda: [self.record]
        registry.get.side_effect = lambda _id: self.record
        self.fleet = SimpleNamespace(hosts={"one": first, "two": second}, registry=registry,
                                     jobs={}, jobs_lock=threading.RLock())
        self.fleet.submit = Mock(side_effect=self.submit)
        self.time = 100
        self.sync = TemplateSync(self.fleet, clock=lambda: self.time, retry_seconds=60)

    def submit(self, host_id, operation, **params):
        job_id = f"{len(self.fleet.jobs) + 1:032x}"
        job = {"id": job_id, "host_id": host_id, "operation": operation, "name": params["template"],
               "status": "queued", "template_digest": self.record["manifest_digest"]}
        self.fleet.jobs[job_id] = job
        return dict(job)

    def complete(self, host_id):
        for job in self.fleet.jobs.values():
            if job["host_id"] == host_id and job["status"] == "queued":
                job["status"] = "succeeded"
        self.record["deployments"] = [receipt for receipt in self.record["deployments"] if receipt["host_id"] != host_id]
        self.record["deployments"].append({"host_id": host_id, "manifest_digest": self.record["manifest_digest"],
                                           "host_binding_digest": host_binding_digest(self.fleet.hosts[host_id])})

    def test_registry_registration_synchronizes_every_node_once(self):
        self.sync.reconcile()
        self.sync.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 2)
        self.assertEqual({node["status"] for node in self.sync.summary(self.record)["nodes"]}, {"syncing"})
        self.complete("one")
        self.complete("two")
        self.sync.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 2)
        self.assertEqual({node["status"] for node in self.sync.summary(self.record)["nodes"]}, {"ready"})

    def test_new_node_and_new_revision_converge_without_install_target_selection(self):
        self.sync.reconcile()
        self.complete("one")
        self.complete("two")
        self.fleet.hosts["three"] = replace(self.fleet.hosts["one"], id="three", target="host-three")
        self.sync.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 3)
        self.assertEqual(self.fleet.submit.call_args.args[0], "three")
        self.complete("three")
        self.record.update(revision=2, manifest_digest="digest-two")
        self.sync.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 6)
        self.assertEqual(self.sync.summary(self.record)["desired_revision"], 2)

    def test_capacity_failure_is_visible_and_retried_without_blocking_other_nodes(self):
        self.sync.reconcile()
        failed = next(job for job in self.fleet.jobs.values() if job["host_id"] == "one")
        failed.update(status="failed", error="Insufficient capacity.")
        self.complete("two")
        self.sync.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 2)
        summary = {node["host_id"]: node for node in self.sync.summary(self.record)["nodes"]}
        self.assertEqual(summary["one"]["status"], "failed")
        self.assertEqual(summary["one"]["error"], "Insufficient capacity.")
        self.assertEqual(summary["two"]["status"], "ready")
        self.time += 61
        self.sync.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 3)

    def test_global_retry_clears_backoff_and_changed_host_binding_reinstalls(self):
        self.sync.reconcile()
        for job in self.fleet.jobs.values():
            job.update(status="failed", error="Host offline.")
        self.sync.reconcile()
        self.assertEqual(self.sync.request("native"), {"template": "native", "requested": True})
        self.sync.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 4)
        self.complete("one")
        self.complete("two")
        self.fleet.hosts["one"] = replace(self.fleet.hosts["one"], pool="new-pool")
        self.sync.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 5)
        self.assertEqual(self.fleet.submit.call_args.args[0], "one")

    def test_restart_uses_receipts_and_in_flight_jobs_without_duplicate_mutations(self):
        self.sync.reconcile()
        self.complete("one")
        restarted = TemplateSync(self.fleet)
        restarted.reconcile()
        self.assertEqual(self.fleet.submit.call_count, 2)

    def test_queue_failure_on_one_node_does_not_drop_other_nodes(self):
        def submit(host_id, operation, **params):
            if host_id == "one":
                raise ValueError("queue full")
            return self.submit(host_id, operation, **params)
        self.fleet.submit.side_effect = submit
        self.sync.reconcile()
        self.assertEqual(len(self.fleet.jobs), 1)
        statuses = {node["host_id"]: node["status"] for node in self.sync.summary(self.record)["nodes"]}
        self.assertEqual(statuses, {"one": "failed", "two": "syncing"})


if __name__ == "__main__":
    unittest.main()
