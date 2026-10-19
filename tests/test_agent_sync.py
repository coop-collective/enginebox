"""Controller fan-out preserves private assignment and host lifecycle ordering."""
from contextlib import contextmanager
from types import SimpleNamespace
import threading
import unittest
from unittest.mock import Mock, patch

from agent_sync import AgentSync


class AgentSyncTests(unittest.TestCase):
    def setUp(self):
        self.host = SimpleNamespace(id="one", project="private", pool="pool")
        self.fleet = SimpleNamespace(agents=Mock(), hosts={"one": self.host},
                                     locks={"one": threading.Lock()}, attach_agents=Mock())
        self.sync = AgentSync(self.fleet)
        self.vm = {"name": "ebox-work", "project": "private", "pool": "pool", "managed": True, "status": "Running"}

    def test_only_running_assigned_workspaces_receive_credentials(self):
        self.assertTrue(self.sync.eligible(self.host, self.vm))
        for change in [{"project": "other"}, {"pool": "other"}, {"managed": False},
                       {"status": "Frozen"}, {"status": "Stopped"}, {"warm_pool_state": "ready"},
                       {"warm_pool_state": "preparing"}, {"warm_pool_state": "quarantined"}]:
            self.assertFalse(self.sync.eligible(self.host, self.vm | change))
        self.assertTrue(self.sync.eligible(self.host, self.vm | {"warm_pool_state": "claimed"}))

    def test_discovery_holds_lifecycle_locks_and_covers_new_nodes(self):
        held = set()
        @contextmanager
        def host_lock(host):
            held.add(host.id)
            try:
                yield
            finally:
                held.remove(host.id)
        def attach(host_id, name):
            self.assertIn(host_id, held)
            self.assertTrue(self.fleet.locks[host_id].locked())
        self.fleet.attach_agents.side_effect = attach
        with patch("fleet.host_operation_lock", side_effect=host_lock), patch("fleet.inventory", return_value={"instances": [self.vm]}):
            self.sync.reconcile()
            self.fleet.hosts["two"] = SimpleNamespace(id="two", project="private", pool="pool")
            self.fleet.locks["two"] = threading.Lock()
            self.sync.reconcile()
        self.assertEqual([call.args for call in self.fleet.attach_agents.call_args_list],
                         [("one", "ebox-work"), ("one", "ebox-work"), ("two", "ebox-work")])
        self.assertFalse(held)

    def test_busy_or_offline_node_does_not_touch_credentials(self):
        with patch("fleet.inventory") as inventory:
            with self.fleet.locks["one"]:
                self.sync.reconcile()
            inventory.assert_not_called()
        with patch("fleet.host_operation_lock", side_effect=RuntimeError("offline")):
            self.sync.reconcile()
        self.fleet.attach_agents.assert_not_called()
        self.assertFalse(self.fleet.locks["one"].locked())
