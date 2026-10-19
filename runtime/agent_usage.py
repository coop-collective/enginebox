"""Read-only, account-level native allowance observations.

Only normalized quota fields leave this module. No account identity, raw native
output, auth token, prompt, monetary estimate or reset redemption is collected.
"""
from __future__ import annotations

from copy import deepcopy
from datetime import datetime, timezone
import json
import math
import os
import re
import selectors
import subprocess
import threading
import time

from guest_auth import stop_group


REFRESH_SECONDS = 60
STALE_SECONDS = 300
MAX_OUTPUT_BYTES = 262144


def _iso(seconds):
    return datetime.fromtimestamp(seconds, timezone.utc).isoformat()


def unavailable(provider, detail=None):
    messages = {
        "codex": "Connect a ChatGPT account to read its reported allowance.",
        "claude": "Claude does not provide an on-demand allowance through this integration. Check provider usage.",
        "cursor": "Cursor does not expose personal allowance through the supported CLI. Check provider usage.",
    }
    return {"provider": provider, "scope": "controller_account", "status": "unavailable",
            "detail": detail or messages[provider], "source": None, "observed_at": None,
            "freshness": "unknown", "plan": None, "billing_cycle": None,
            "allowance_windows": [], "credits": [], "refresh_after_seconds": REFRESH_SECONDS}


def _number(value, minimum=0, maximum=1e15):
    return type(value) in {int, float} and math.isfinite(value) and minimum <= value <= maximum


def _label(value, fallback):
    return value if isinstance(value, str) and re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_. /+-]{0,79}", value) else fallback


def normalize_codex(value, now):
    """Prefer the multi-bucket view; never sum repeated account allowances."""
    if not isinstance(value, dict):
        raise ValueError("Unsupported native usage response.")
    buckets = value.get("rateLimitsByLimitId")
    if not isinstance(buckets, dict) or not buckets:
        single = value.get("rateLimits")
        buckets = {"codex": single} if isinstance(single, dict) else {}
    if not buckets or len(buckets) > 16:
        raise ValueError("Unsupported native usage response.")
    result = unavailable("codex")
    result.update(source="Codex native app-server", observed_at=_iso(now), freshness="current")
    for key, bucket in buckets.items():
        if not isinstance(bucket, dict):
            continue
        bucket_id = _label(bucket.get("limitId"), _label(key, "codex"))
        label = _label(bucket.get("limitName"), bucket_id)
        plan = bucket.get("planType")
        if isinstance(plan, str) and re.fullmatch(r"[a-zA-Z][a-zA-Z0-9_-]{0,39}", plan):
            result["plan"] = result["plan"] or plan
        for position in ("primary", "secondary"):
            window = bucket.get(position)
            if not isinstance(window, dict) or not _number(window.get("usedPercent"), maximum=100):
                continue
            reset = window.get("resetsAt")
            if not _number(reset, 1, 253402300799):
                continue  # Without a reset we cannot establish a current allowance.
            duration = window.get("windowDurationMins")
            result["allowance_windows"].append({"id": bucket_id + ":" + position, "label": label,
                "used_percentage": window["usedPercent"], "resets_at": _iso(reset),
                "window_minutes": duration if _number(duration, 1, 525600) else None})
        credit = bucket.get("credits")
        if isinstance(credit, dict):
            balance = credit.get("balance")
            # Keep the provider's credit unit. This is never presented as dollars.
            if _number(balance):
                balance = str(balance)
            if not isinstance(balance, str) or not re.fullmatch(r"\d{1,15}(?:\.\d{1,8})?", balance):
                balance = None
            unlimited = credit.get("unlimited") is True
            if balance is not None or unlimited:
                result["credits"].append({"label": label, "balance": balance, "unlimited": unlimited,
                                          "unit": "provider credits"})
    if result["allowance_windows"] or result["credits"]:
        result.update(status="available", detail="Reported by the provider for this connected account.")
    else:
        result.update(detail="The provider did not report allowance windows or a credit balance.")
    return result


def read_codex(executable, environment, *, timeout=12):
    """Only initialize and account/rateLimits/read are sent to native stdio."""
    process = subprocess.Popen([executable, "-c", 'cli_auth_credentials_store="file"', "app-server"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        env=environment, cwd=environment["HOME"], start_new_session=True)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    buffer, total, deadline, initialized = b"", 0, time.monotonic() + timeout, False

    def send(message):
        process.stdin.write(json.dumps(message, separators=(",", ":")).encode() + b"\n")
        process.stdin.flush()

    try:
        send({"id": 1, "method": "initialize", "params": {
            "clientInfo": {"name": "enginebox-usage", "version": "0.1.0"}}})
        while time.monotonic() < deadline:
            for key, _ in selector.select(.1):
                chunk = os.read(key.fd, 4096)
                total += len(chunk)
                if not chunk or total > MAX_OUTPUT_BYTES:
                    raise ValueError("Unsupported native usage response.")
                buffer += chunk
                while b"\n" in buffer:
                    line, buffer = buffer.split(b"\n", 1)
                    message = json.loads(line)
                    if not isinstance(message, dict):
                        raise ValueError("Unsupported native usage response.")
                    if message.get("id") == 1 and not initialized:
                        if "error" in message or not isinstance(message.get("result"), dict):
                            raise ValueError("Native usage method is unavailable.")
                        initialized = True
                        send({"method": "initialized"})
                        send({"id": 2, "method": "account/rateLimits/read"})
                    elif message.get("id") == 2 and initialized:
                        if "error" in message:
                            raise ValueError("Native usage method is unavailable.")
                        return normalize_codex(message.get("result"), time.time())
        raise TimeoutError("Native usage request timed out.")
    finally:
        selector.close()
        process.stdin.close()
        stop_group(process)
        process.stdout.close()


class UsageCache:
    """One observation per connected account, with bounded retry frequency."""
    def __init__(self):
        self._lock = threading.Lock()
        self._entries = {}

    def read(self, provider, fingerprint, loader):
        with self._lock:
            now = time.time()
            entry = self._entries.get(provider)
            if entry and entry["fingerprint"] != fingerprint:
                entry = None  # Never show an earlier account's allowance after reconnect.
            if not entry or now - entry["attempted_at"] >= REFRESH_SECONDS:
                try:
                    snapshot = loader()
                except (OSError, ValueError, TypeError, TimeoutError, subprocess.SubprocessError):
                    snapshot = deepcopy(entry["snapshot"]) if entry else unavailable(provider)
                    snapshot.update(status="unavailable", freshness="stale" if snapshot["observed_at"] else "unknown",
                        detail="Usage could not be refreshed. Check the provider or try again shortly.")
                entry = {"fingerprint": fingerprint, "attempted_at": now, "snapshot": snapshot}
                self._entries[provider] = entry
            # A quota can reset while the bounded native request is in flight.
            # Evaluate freshness at response time, not at the start of collection.
            now = time.time()
            result = deepcopy(entry["snapshot"])
            observed = result.get("observed_at")
            if observed and now - datetime.fromisoformat(observed).timestamp() >= STALE_SECONDS:
                result["freshness"] = "stale"
            result["refresh_after_seconds"] = max(1, int(REFRESH_SECONDS - (now - entry["attempted_at"])))
            # Expired windows remain last-known observations. Neither the API nor
            # the dashboard turns a passed reset into an invented full allowance.
            for window in result["allowance_windows"]:
                stale = result["freshness"] != "current" or datetime.fromisoformat(window["resets_at"]).timestamp() <= now
                window["freshness"] = "stale" if stale else "current"
                window["remaining_percentage"] = None if stale else max(0, 100 - window["used_percentage"])
            return result
