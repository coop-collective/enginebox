"""Prebooted, never-assigned VM reserves. Native harnesses are not involved."""
from __future__ import annotations

from dataclasses import asdict
from datetime import datetime, timezone
import hashlib
import json
import subprocess
import threading
import time
import uuid

PREFIX = "enginebox-pool:v1:"


class PoolRefillError(RuntimeError):
    """A refill failure already recorded against its individual policy."""


def reservation(state, **fields):
    if state not in {"claimed", "quarantined"}:
        raise ValueError("Invalid pool reservation state.")
    return PREFIX + json.dumps({"state": state, **fields}, sort_keys=True, separators=(",", ":"))


def generation(host, template, resources, snapshot):
    # Bind the snapshot's creation identity and effective guest configuration.
    # Incus also returns observational fields such as last_used_at, size and
    # expires_at; reading/using the same snapshot must not invalidate its pool.
    # Expanded settings matter because inherited profile changes affect clones.
    snapshot_identity = {key: snapshot.get(key) for key in (
        "name", "created_at", "architecture", "type", "stateful", "ephemeral",
        "config", "devices", "expanded_config", "expanded_devices", "profiles")}
    template_identity = {key: template.get(key) for key in ("id", "source", "snapshot", "revision", "manifest_digest")}
    value = {"template": template_identity, "resources": resources, "snapshot": snapshot_identity,
             "pool": host.pool, "project": host.project, "network": getattr(host, "network", None),
             "guest": asdict(host.guest)}
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def eligible(vm, host, template_id, resources, expected_generation):
    return (vm.get("project") == host.project and vm.get("pool") == host.pool
            and vm.get("managed") is True and vm.get("warm_pool_state") == "ready"
            and not vm.get("pool_claim") and vm.get("status") == "Frozen"
            and vm.get("template") == template_id and vm.get("pool_generation") == expected_generation
            and vm.get("cpu") == resources["cpu"]
            and vm.get("memory_bytes") == resources["memory_gib"] * 1024**3
            and vm.get("disk_bytes") == resources["disk_gib"] * 1024**3
            and vm.get("state_disk_bytes", 0) >= (resources["memory_gib"] + 2) * 1024**3)


class WarmPool:
    def __init__(self, fleet):
        self.fleet = fleet
        self.stopping = threading.Event()
        self.events = {key: threading.Event() for key in fleet.hosts}
        self.threads = []
        self.status_lock = threading.Lock()
        self.errors = {}
        self.host_errors = {}

    def start(self):
        for host in self.fleet.hosts.values():
            if host.warm_pools:
                thread = threading.Thread(target=self._loop, args=(host,), daemon=True, name=f"warm-{host.id}")
                self.threads.append(thread)
                thread.start()

    def stop(self):
        self.stopping.set()
        for event in self.events.values():
            event.set()
        for thread in self.threads:
            thread.join(timeout=5)

    def wake(self, host_id):
        self.events[host_id].set()

    def _error(self, host, message, policy=None):
        with self.status_lock:
            errors = self.host_errors if policy is None else self.errors
            key = host.id if policy is None else (host.id, policy["template"], policy["size"])
            if message:
                errors[key] = message[:500]
            else:
                errors.pop(key, None)

    def _loop(self, host):
        while not self.stopping.is_set():
            self.events[host.id].clear()
            try:
                changed = self.reconcile_once(host)
                if changed:
                    continue
            except PoolRefillError:
                # The failure belongs to one policy, not every pool on the host.
                pass
            except Exception as exc:
                self._error(host, str(exc)[:500])
            self.events[host.id].wait(15)

    def summary(self, host, facts):
        result = []
        for policy in host.warm_pools:
            record = {"template": policy["template"], "size": policy["size"], "target": policy["ready"],
                      "ready": 0, "preparing": 0}
            try:
                template = self.fleet.template(host, policy["template"])
                resources = self.fleet.resources(host, template, policy["size"])
                snapshot = self.fleet.template_snapshot(host, template)
                expected = generation(host, template, resources, snapshot)
                record["resources"] = resources
                members = [vm for vm in facts["instances"] if vm.get("project") == host.project
                           and vm.get("pool_generation") == expected and not vm.get("pool_claim")]
                record["ready"] = sum(eligible(vm, host, template["id"], resources, expected) for vm in members)
                record["preparing"] = sum(vm.get("warm_pool_state") == "preparing" for vm in members)
                with self.status_lock:
                    error = self.errors.get((host.id, policy["template"], policy["size"])) or self.host_errors.get(host.id)
                    if error:
                        record["error"] = error
            except (ValueError, RuntimeError, OSError, subprocess.SubprocessError) as exc:
                record["error"] = str(exc)[:500]
            result.append(record)
        return result

    def allocate(self, host, name, template_id, size=None, resources=None, *, job_id):
        # Caller holds both the controller mutex and the host-wide lock.
        from fleet import inventory
        facts = inventory(host)
        self.fleet.check_name(host, name, facts)
        template = self.fleet.template(host, template_id)
        selected = self.fleet.resources(host, template, size, resources)
        snapshot = self.fleet.template_snapshot(host, template)
        expected = generation(host, template, selected, snapshot)
        slot = next((vm for vm in facts["instances"] if eligible(vm, host, template_id, selected, expected)), None)
        if slot is None:
            result = self.fleet._create(host, name, template_id, size, resources=resources)
            return result | {"workspace_name": name, "startup": "cold"}
        actual = slot["name"]
        box = host.box()
        started = time.monotonic()
        # description is a first-class Incus property: unlike user.* config it
        # does not notify a frozen guest agent. Commit the reservation BEFORE
        # unfreezing. It survives controller crashes and is visible to all hosts.
        box.run("config", "set", actual, "--property", "description=" + reservation(
            "claimed", workspace_name=name, job_id=job_id, generation=expected,
            claimed_at=datetime.now(timezone.utc).isoformat()))
        try:
            box.lifecycle(actual, "resume")
            box.run("config", "set", actual, "user.enginebox.pool.state=claimed", f"user.enginebox.workspace={name}")
            readiness = self.fleet._wait_ready(box, actual)
        except Exception as exc:
            raise RuntimeError(f"Workspace reserved as {actual}; inspect and resume that instance. It will never return to the pool. {exc}") from exc
        self.wake(host.id)
        return {"name": actual, "workspace_name": name, "status": "Running", "startup": "warm",
                "copy_seconds": 0, "ready_seconds": round(time.monotonic() - started, 3), "readiness": readiness}

    def _quarantine(self, host, name, reason):
        host.box().run("config", "set", name, "--property", "description=" + reservation("quarantined", reason=reason))

    def reconcile_once(self, host):
        """Create at most one slot. Long readiness waits never hold the host lock."""
        from fleet import inventory, host_operation_lock, resource_fits
        lock = self.fleet.locks[host.id]
        if not lock.acquire(blocking=False):
            return False
        slot = None
        try:
            with host_operation_lock(host):
                facts = inventory(host)
                self._error(host, "")
                for policy in host.warm_pools:
                    if not policy["ready"]:
                        self._error(host, "", policy)
                        continue
                    try:
                        template = self.fleet.template(host, policy["template"])
                        resources = self.fleet.resources(host, template, policy["size"])
                        snapshot = self.fleet.template_snapshot(host, template)
                        expected = generation(host, template, resources, snapshot)
                    except (ValueError, RuntimeError, OSError, subprocess.SubprocessError) as exc:
                        self._error(host, str(exc), policy)
                        continue
                    members = [vm for vm in facts["instances"] if vm.get("project") == host.project
                               and vm.get("managed") and vm.get("pool") == host.pool
                               and vm.get("template") == template["id"] and vm.get("pool_size") == policy["size"]
                               and vm.get("warm_pool_state") in {"ready", "preparing", "quarantined"}]
                    for vm in members:
                        if vm.get("pool_claim"):
                            continue
                        state = vm.get("warm_pool_state")
                        stale = vm.get("pool_generation") != expected or (state == "ready" and vm["status"] != "Frozen")
                        created = vm.get("pool_created_at", "")
                        if state == "preparing":
                            try:
                                stale |= (datetime.now(timezone.utc) - datetime.fromisoformat(created)).total_seconds() > 600
                            except (ValueError, TypeError):
                                stale = True
                        if stale and state != "quarantined":
                            self._quarantine(host, vm["name"], "Stale or interrupted unassigned reserve; inspect before removal.")
                            vm["warm_pool_state"] = "quarantined"
                    failed = [vm for vm in members if vm.get("pool_generation") == expected and vm.get("warm_pool_state") == "quarantined"]
                    if failed:
                        self._error(host, f"Unassigned reserve {failed[0]['name']} needs inspection; automatic refill is paused for this pool.", policy)
                        continue
                    ready = sum(eligible(vm, host, template["id"], resources, expected) for vm in members)
                    preparing = sum(vm.get("warm_pool_state") == "preparing" and vm.get("pool_generation") == expected for vm in members)
                    if ready + preparing >= policy["ready"]:
                        self._error(host, "", policy)
                        continue
                    if not resource_fits(host, facts, resources):
                        self._error(host, "Warm-pool refill is waiting for CPU, RAM, disk or instance capacity.", policy)
                        continue
                    name = "ebox-warm-" + uuid.uuid4().hex[:16]
                    slot = (name, expected, policy)
                    try:
                        self.fleet._create(host, name, template["id"], policy["size"],
                            pool_metadata={"state": "preparing", "generation": expected, "size": policy["size"],
                                           "created_at": datetime.now(timezone.utc).isoformat()}, wait=False)
                    except Exception as exc:
                        self._error(host, str(exc), policy)
                        raise PoolRefillError(str(exc)) from exc
                    break
        finally:
            lock.release()
        if slot is None:
            return False
        name, expected, policy = slot
        try:
            self.fleet._wait_ready(host.box(), name)
            with lock, host_operation_lock(host):
                facts = inventory(host)
                vm = next((vm for vm in facts["instances"] if vm["project"] == host.project and vm["name"] == name), None)
                if (not vm or vm.get("warm_pool_state") != "preparing" or vm.get("pool_claim")
                        or vm.get("pool_generation") != expected or vm["status"] != "Running"):
                    raise RuntimeError("Prepared reserve changed while booting; inspect it before reuse.")
                # Publish before freeze while guest metadata notifications work.
                # Eligibility also requires Frozen, and the shared lock excludes
                # claims until the transition is complete.
                host.box().run("config", "set", name, "user.enginebox.pool.state=ready")
                host.box().lifecycle(name, "pause")
            self._error(host, "", policy)
            return True
        except Exception as exc:
            self._error(host, str(exc), policy)
            try:
                with lock, host_operation_lock(host):
                    facts = inventory(host)
                    vm = next((vm for vm in facts["instances"] if vm["project"] == host.project and vm["name"] == name), None)
                    if vm and not vm.get("pool_claim") and vm.get("warm_pool_state") in {"preparing", "ready"}:
                        self._quarantine(host, name, "Preparation failed; inspect this unassigned reserve before removal.")
            except Exception as cleanup_error:
                message = f"{exc}; reserve {name} could not be quarantined: {cleanup_error}"
                self._error(host, message, policy)
                raise PoolRefillError(message) from cleanup_error
            raise PoolRefillError(str(exc)) from exc
