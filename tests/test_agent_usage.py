"""Native quota reads use synthetic subprocesses and credentials only."""
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

from agent_usage import UsageCache, normalize_codex, read_codex
from agent_access import AgentAccess, NATIVE_FILES, write_private


NOW = 1792411200


def response(used=25, reset=NOW + 3600):
    return {"rateLimits": {"primary": {"usedPercent": used, "windowDurationMins": 10080,
                                       "resetsAt": reset}, "secondary": None,
                            "credits": {"hasCredits": True, "unlimited": False, "balance": "12.50"}}}


class UsageTests(unittest.TestCase):
    def test_only_valid_reported_fields_survive_and_no_billing_cycle_is_invented(self):
        value = response()
        value["tokens"] = {"refreshToken": "private-secret"}
        value["rateLimits"]["email"] = "private@example.invalid"
        normalized = normalize_codex(value, NOW)
        self.assertEqual(normalized["allowance_windows"][0]["used_percentage"], 25)
        self.assertEqual(normalized["credits"][0]["balance"], "12.50")
        self.assertEqual(normalized["credits"][0]["unit"], "provider credits")
        self.assertIsNone(normalized["billing_cycle"])
        self.assertNotIn("private", json.dumps(normalized))

    def test_multibucket_view_is_not_added_to_duplicate_legacy_view(self):
        value = response()
        value["rateLimitsByLimitId"] = {"codex": value["rateLimits"], "extra": {
            "limitName": "Extra model", "primary": {"usedPercent": 50, "resetsAt": NOW + 100},
            "secondary": {"usedPercent": 75, "resetsAt": NOW + 500}}}
        normalized = normalize_codex(value, NOW)
        self.assertEqual(len(normalized["allowance_windows"]), 3)
        self.assertEqual(len(normalized["credits"]), 1)
        self.assertEqual(normalized["allowance_windows"][1]["label"], "Extra model")

    def test_missing_malformed_and_context_values_cannot_become_a_balance(self):
        for invalid in [True, -1, 101, float("nan"), "25", None]:
            value = response(invalid)
            value["rateLimits"]["credits"] = {"balance": "private-token", "unlimited": "true"}
            value["context_window"] = {"remaining_percentage": 99}
            normalized = normalize_codex(value, NOW)
            self.assertEqual(normalized["status"], "unavailable")
            self.assertEqual(normalized["allowance_windows"], [])
            self.assertEqual(normalized["credits"], [])
        value = response()
        del value["rateLimits"]["primary"]["resetsAt"]
        self.assertEqual(normalize_codex(value, NOW)["allowance_windows"], [])
        for invalid in [None, [], {}, {"rateLimits": "wrong"}]:
            with self.assertRaises(ValueError):
                normalize_codex(invalid, NOW)

    def test_cache_bounds_polling_and_invalidates_after_account_change(self):
        cache = UsageCache()
        loader = Mock(side_effect=lambda: normalize_codex(response(), NOW))
        with patch("agent_usage.time.time", return_value=NOW):
            with ThreadPoolExecutor(max_workers=8) as pool:
                values = list(pool.map(lambda _: cache.read("codex", "account-a", loader), range(8)))
            self.assertEqual(loader.call_count, 1)
            self.assertEqual(values[0]["allowance_windows"][0]["remaining_percentage"], 75)
            cache.read("codex", "account-b", loader)
            self.assertEqual(loader.call_count, 2)

    def test_reset_and_failed_refresh_make_old_numbers_stale(self):
        cache = UsageCache()
        with patch("agent_usage.time.time", return_value=NOW):
            cache.read("codex", "account-a", lambda: normalize_codex(response(reset=NOW + 10), NOW))
        with patch("agent_usage.time.time", return_value=NOW + 11):
            value = cache.read("codex", "account-a", Mock(side_effect=AssertionError("Must use cache")))
            self.assertIsNone(value["allowance_windows"][0]["remaining_percentage"])
        error = Mock(side_effect=ValueError("private-secret-raw-provider-error"))
        with patch("agent_usage.time.time", return_value=NOW + 61):
            stale = cache.read("codex", "account-a", error)
            self.assertEqual(stale["freshness"], "stale")
            self.assertEqual(stale["status"], "unavailable")
            self.assertNotIn("private-secret", json.dumps(stale))
            fresh_account = cache.read("codex", "account-b", error)
            self.assertEqual(fresh_account["allowance_windows"], [])
            self.assertIsNone(fresh_account["observed_at"])

    def test_reset_during_native_fetch_is_already_stale_when_returned(self):
        cache = UsageCache()
        with patch("agent_usage.time.time", side_effect=[NOW, NOW + 12]):
            result = cache.read("codex", "account-a", lambda: normalize_codex(response(reset=NOW + 10), NOW + 11))
        self.assertEqual(result["allowance_windows"][0]["freshness"], "stale")
        self.assertIsNone(result["allowance_windows"][0]["remaining_percentage"])


class NativeUsageProtocolTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.home = Path(self.temporary.name)
        self.binary = self.home / "codex-fixture"
        self.environment = {"HOME": str(self.home), "PATH": os.defpath}

    def script(self, body):
        self.binary.write_text("#!" + sys.executable + "\nimport sys,json,time\n" + body)
        self.binary.chmod(0o700)

    def test_only_initialization_and_native_rate_read_are_sent(self):
        trace = self.home / "methods.jsonl"
        self.script("""
for line in sys.stdin:
    message = json.loads(line)
    with open(TRACE, "a") as output: output.write(message["method"] + "\\n")
    if message.get("id") == 1:
        print(json.dumps({"id": 1, "result": {"userAgent": "fixture"}}), flush=True)
    if message.get("id") == 2:
        print(json.dumps({"id": 2, "result": RESPONSE}), flush=True)
""".replace("TRACE", repr(str(trace))).replace("RESPONSE", repr(response())))
        value = read_codex(str(self.binary), self.environment)
        self.assertEqual(trace.read_text().splitlines(), ["initialize", "initialized", "account/rateLimits/read"])
        self.assertEqual(value["status"], "available")

    def test_unsupported_native_method_never_leaks_provider_error(self):
        self.script('print(json.dumps({"id":1,"error":{"message":"private-provider-output"}}),flush=True)\ntime.sleep(5)\n')
        with self.assertRaisesRegex(ValueError, "Native usage method is unavailable") as caught:
            read_codex(str(self.binary), self.environment)
        self.assertNotIn("private", str(caught.exception))

    def test_timeout_and_output_budget_terminate_native_process(self):
        self.script("time.sleep(5)\n")
        start = time.monotonic()
        with self.assertRaises(TimeoutError):
            read_codex(str(self.binary), self.environment, timeout=.1)
        self.assertLess(time.monotonic() - start, 3)
        self.script('sys.stdout.write("x" * 300000); sys.stdout.flush(); time.sleep(5)\n')
        with self.assertRaises(ValueError):
            read_codex(str(self.binary), self.environment)

    def test_controller_does_not_launch_for_missing_api_key_or_unsupported_accounts(self):
        agents = AgentAccess(Mock(), self.home / "agents", binaries={p: str(self.binary) for p in ("codex", "claude", "cursor")})
        self.addCleanup(agents.close)
        with patch("agent_access.read_codex") as read:
            for provider in ("codex", "claude", "cursor"):
                self.assertEqual(agents.usage(provider)["status"], "unavailable")
            home = agents.home("codex")
            write_private(home / NATIVE_FILES["codex"], {"OPENAI_API_KEY": "private-api-key"}, home)
            self.assertEqual(agents.usage("codex")["status"], "unavailable")
            read.assert_not_called()
        with self.assertRaises(ValueError):
            agents.usage("arbitrary")
