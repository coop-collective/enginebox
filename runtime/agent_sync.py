"""Deliver controller-owned native connections to assigned private workspaces."""
from __future__ import annotations

import threading


class AgentSync:
    def __init__(self, fleet, *, interval=30):
        self.fleet = fleet
        self.interval = interval
        self._wake = threading.Event()
        self._stop = threading.Event()
        self._thread = None

    @staticmethod
    def eligible(host, vm):
        return (vm.get("project") == host.project and vm.get("pool") == host.pool
                and vm.get("managed") and vm.get("status") == "Running"
                and vm.get("warm_pool_state") not in {"preparing", "ready", "quarantined"})

    def reconcile(self):
        if not self.fleet.agents:
            return
        # The same attachment operation checks identity, refresh ownership and
        # manual credential changes. Discovery includes workspaces made before
        # a connection was added, and hosts registered on subsequent startups.
        from fleet import inventory, host_operation_lock
        for host in list(self.fleet.hosts.values()):
            if self._stop.is_set():
                break
            lock = self.fleet.locks[host.id]
            if not lock.acquire(blocking=False):
                continue
            try:
                with host_operation_lock(host):
                    for vm in inventory(host)["instances"]:
                        if self._stop.is_set():
                            break
                        if self.eligible(host, vm):
                            self.fleet.attach_agents(host.id, vm["name"])
            except Exception:
                # A busy/offline host retries without blocking other nodes.
                pass
            finally:
                lock.release()

    def wake(self):
        self._wake.set()

    def _run(self):
        while not self._stop.is_set():
            self._wake.clear()
            self.reconcile()
            self._wake.wait(self.interval)

    def start(self):
        if self._thread is None and self.fleet.agents:
            self._thread = threading.Thread(target=self._run, name="enginebox-agent-sync", daemon=True)
            self._thread.start()

    def stop(self):
        self._stop.set()
        self.wake()
        if self._thread:
            self._thread.join(timeout=5)
