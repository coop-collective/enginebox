"""Reconcile controller template manifests onto all registered fleet nodes."""
from __future__ import annotations

import threading
import time

from templates import host_binding_digest


class TemplateSync:
    def __init__(self, fleet, *, interval=15, retry_seconds=60, clock=time.monotonic):
        self.fleet = fleet
        self.interval = interval
        self.retry_seconds = retry_seconds
        self.clock = clock
        self._lock = threading.RLock()
        self._attempts = {}
        self._wake = threading.Event()
        self._stop = threading.Event()
        self._thread = None

    def _jobs(self):
        with self.fleet.jobs_lock:
            return [dict(job) for job in self.fleet.jobs.values() if job["operation"] == "install_template"]

    @staticmethod
    def _current(record, host):
        return any(receipt["host_id"] == host.id
                   and receipt["manifest_digest"] == record["manifest_digest"]
                   and receipt["host_binding_digest"] == host_binding_digest(host)
                   for receipt in record["deployments"])

    @staticmethod
    def _key(record, host):
        return (host.id, record["id"], record["manifest_digest"], host_binding_digest(host))

    def summary(self, record):
        jobs = self._jobs()
        nodes = []
        with self._lock:
            for host in self.fleet.hosts.values():
                node = {"host_id": host.id, "label": host.label, "status": "pending"}
                relevant = [job for job in jobs if job["host_id"] == host.id and job["name"] == record["id"]]
                active = next((job for job in reversed(relevant) if job["status"] in {"queued", "running"}), None)
                if self._current(record, host):
                    node["status"] = "ready"
                elif active:
                    node.update(status="syncing", job_id=active["id"])
                else:
                    attempt = self._attempts.get(self._key(record, host), {})
                    failed = next((job for job in reversed(relevant)
                                   if job["status"] == "failed" and job.get("template_digest") == record["manifest_digest"]), None)
                    if attempt.get("error") or failed:
                        node.update(status="failed", error=attempt.get("error") or failed.get("error", "Template installation failed."))
                        if failed:
                            node["job_id"] = failed["id"]
                nodes.append(node)
        return {"desired_revision": record["revision"], "nodes": nodes}

    def request(self, template_id):
        if not self.fleet.registry:
            raise ValueError("Configure a persistent template registry first.")
        self.fleet.registry.get(template_id)
        with self._lock:
            for key in list(self._attempts):
                if key[1] == template_id:
                    self._attempts.pop(key)
        self.wake()
        return {"template": template_id, "requested": True}

    def wake(self):
        self._wake.set()

    def reconcile(self):
        if not self.fleet.registry or self._stop.is_set():
            return
        # Also covers manual sync requests and prevents two controller threads
        # from enqueueing the same node/revision between queue observations.
        with self._lock:
            records = self.fleet.registry.list()
            jobs = self._jobs()
            active_hosts = {job["host_id"] for job in jobs if job["status"] in {"queued", "running"}}
            valid = {self._key(record, host) for record in records for host in self.fleet.hosts.values()}
            self._attempts = {key: value for key, value in self._attempts.items() if key in valid}
            for record in records:
                for host in self.fleet.hosts.values():
                    key = self._key(record, host)
                    if self._current(record, host):
                        self._attempts.pop(key, None)
                        continue
                    if host.id in active_hosts:
                        continue
                    attempt = self._attempts.get(key, {})
                    if attempt.get("job_id"):
                        previous = next((job for job in jobs if job["id"] == attempt["job_id"]), None)
                        if previous and previous["status"] == "failed" and not attempt.get("failed_observed"):
                            attempt.update(failed_observed=True, next_retry=self.clock() + self.retry_seconds,
                                           error=previous.get("error", "Template installation failed."))
                    if self.clock() < attempt.get("next_retry", 0):
                        continue
                    try:
                        job = self.fleet.submit(host.id, "install_template", template=record["id"])
                        self._attempts[key] = {"job_id": job["id"], "next_retry": 0}
                        active_hosts.add(host.id)
                    except Exception:
                        self._attempts[key] = {"next_retry": self.clock() + self.retry_seconds,
                                               "error": "Template sync could not be queued; it will retry automatically."}

    def _run(self):
        while not self._stop.is_set():
            self._wake.clear()
            try:
                self.reconcile()
            except Exception:
                # A transient registry/queue error must not terminate convergence.
                pass
            self._wake.wait(self.interval)

    def start(self):
        if self._thread is None and self.fleet.registry:
            self._thread = threading.Thread(target=self._run, name="enginebox-template-sync", daemon=True)
            self._thread.start()

    def stop(self):
        self._stop.set()
        self._wake.set()
        if self._thread is not None:
            self._thread.join(timeout=5)
