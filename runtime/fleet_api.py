"""Loopback, bearer-authenticated fleet API for the dashboard and MCP client."""
from __future__ import annotations

import argparse
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import signal
import sys

from fleet import Fleet, load_hosts


ACCOUNT_PATH = r"/v1/hosts/([a-z0-9][a-z0-9-]{0,48})/instances/(ebox-[a-z0-9][a-z0-9-]{0,48})/accounts"
ACCOUNT_SESSION_PATH = ACCOUNT_PATH + r"/(codex|claude|cursor)/sessions/([a-f0-9]{32})"
AGENT_PATH = r"/v1/agents/(codex|claude|cursor)"
AGENT_SESSION_PATH = AGENT_PATH + r"/sessions/([a-f0-9]{32})"


def api_token() -> str:
    path = os.environ.get("ENGINEBOX_API_TOKEN_FILE")
    value = Path(path).read_text().strip() if path else os.environ.get("ENGINEBOX_API_TOKEN", "")
    if len(value) < 32 or any(not 33 <= ord(char) <= 126 for char in value):
        raise ValueError("Set ENGINEBOX_API_TOKEN or ENGINEBOX_API_TOKEN_FILE to a random token of at least 32 characters.")
    return value


def handler_for(fleet: Fleet, token: str):
    class Handler(BaseHTTPRequestHandler):
        server_version = "EngineBox"

        def setup(self):
            super().setup()
            self.connection.settimeout(10)

        def log_message(self, *_args):
            # Access tokens, job payloads and provider output never enter access logs.
            pass

        def respond(self, status, body):
            content = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(content)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.end_headers()
            self.wfile.write(content)

        def authorized(self):
            if self.headers.get("Origin"):
                self.respond(403, {"error": "Browser requests must use the authenticated dashboard proxy."})
                return False
            supplied = self.headers.get("Authorization", "")
            if not hmac.compare_digest(supplied.encode(), ("Bearer " + token).encode()):
                self.respond(401, {"error": "Authentication required."})
                return False
            return True

        def do_GET(self):
            if self.path == "/healthz":
                return self.respond(200, {"ok": True})
            if not self.authorized():
                return
            try:
                if self.path == "/v1/fleet":
                    return self.respond(200, fleet.summary())
                if self.path == "/v1/templates":
                    return self.respond(200, fleet.catalog())
                if self.path == "/v1/agents":
                    return self.respond(200, fleet.agent_access().status())
                central_session = re.fullmatch(AGENT_SESSION_PATH, self.path)
                if central_session:
                    return self.respond(200, {"session": fleet.agent_access().session(*central_session.groups())})
                account = re.fullmatch(ACCOUNT_PATH, self.path)
                if account:
                    return self.respond(200, fleet.accounts.status(account[1], account[2]))
                session = re.fullmatch(ACCOUNT_SESSION_PATH, self.path)
                if session:
                    return self.respond(200, {"session": fleet.accounts.session(*session.groups())})
                match = re.fullmatch(r"/v1/jobs/([a-f0-9]{32})", self.path)
                if match:
                    return self.respond(200, {"job": fleet.job(match[1])})
                self.respond(404, {"error": "Unknown endpoint."})
            except ValueError as exc:
                self.respond(404, {"error": str(exc)})
            except Exception:
                self.respond(500, {"error": "Fleet request failed; inspect service diagnostics."})

        def do_POST(self):
            if not self.authorized():
                return
            try:
                if self.headers.get("Content-Type", "").split(";", 1)[0] != "application/json":
                    return self.respond(415, {"error": "Use application/json."})
                if self.headers.get("Transfer-Encoding"):
                    return self.respond(400, {"error": "Chunked requests are not supported."})
                length = int(self.headers.get("Content-Length", "0"))
                maximum = 1024 * 1024 if self.path == "/v1/templates" else 16384
                if not 1 <= length <= maximum:
                    return self.respond(413, {"error": f"Request must contain 1–{maximum} bytes."})
                body = json.loads(self.rfile.read(length))
                if not isinstance(body, dict):
                    raise ValueError("Body must be an object.")
                central_login = re.fullmatch(AGENT_PATH + r"/login", self.path)
                central_action = re.fullmatch(AGENT_SESSION_PATH + r"/(input|cancel)", self.path)
                if central_login:
                    if body:
                        raise ValueError("Native provider sign-in accepts an empty object.")
                    return self.respond(202, {"session": fleet.agent_access().start(central_login[1])})
                if central_action:
                    provider, session_id, operation = central_action.groups()
                    if operation == "input":
                        if set(body) != {"text"} or not isinstance(body["text"], str):
                            raise ValueError("Provide the one-time authorization code as text.")
                        result = fleet.agent_access().input(provider, session_id, body["text"])
                    else:
                        if body:
                            raise ValueError("Cancel accepts an empty object.")
                        result = fleet.agent_access().cancel(provider, session_id)
                    fleet.agent_sync.wake()
                    return self.respond(200, {"session": result})
                login = re.fullmatch(ACCOUNT_PATH + r"/(codex|claude|cursor)/login", self.path)
                account_action = re.fullmatch(ACCOUNT_SESSION_PATH + r"/(input|cancel)", self.path)
                if login:
                    if body:
                        raise ValueError("Native provider sign-in accepts an empty object.")
                    return self.respond(409, {"error": "Connect agents on the controller through /v1/agents; workspaces receive access automatically."})
                if account_action:
                    host_id, name, provider, session_id, operation = account_action.groups()
                    if operation == "input":
                        if set(body) != {"text"} or not isinstance(body["text"], str):
                            raise ValueError("Provide the one-time authorization code as text.")
                        result = fleet.accounts.input(host_id, name, provider, session_id, body["text"])
                    else:
                        if body:
                            raise ValueError("Cancel accepts an empty object.")
                        result = fleet.accounts.cancel(host_id, name, provider, session_id)
                    return self.respond(200, {"session": result})
                if self.path == "/v1/templates":
                    if not fleet.registry:
                        raise ValueError("Configure a persistent template registry first.")
                    if set(body) == {"manifest"} and isinstance(body["manifest"], dict):
                        record = fleet.registry.register(body["manifest"])
                    elif set(body) == {"url"} and isinstance(body["url"], str):
                        record = fleet.registry.import_url(body["url"])
                    else:
                        raise ValueError("Provide a manifest object or an HTTPS URL.")
                    for host in fleet.hosts:
                        fleet.warm.wake(host)
                    fleet.template_sync.wake()
                    return self.respond(200, {"template": record})
                sync = re.fullmatch(r"/v1/templates/([a-z][a-z0-9-]{0,31})/sync", self.path)
                if sync:
                    if body:
                        raise ValueError("Template synchronization accepts an empty object.")
                    return self.respond(202, fleet.template_sync.request(sync[1]))
                create = re.fullmatch(r"/v1/hosts/([a-z0-9][a-z0-9-]{0,48})/(instances|allocations)", self.path)
                install = re.fullmatch(r"/v1/hosts/([a-z0-9][a-z0-9-]{0,48})/templates/([a-z][a-z0-9-]{0,31})/install", self.path)
                action = re.fullmatch(r"/v1/hosts/([a-z0-9][a-z0-9-]{0,48})/instances/(ebox-[a-z0-9][a-z0-9-]{0,48})/actions", self.path)
                if create:
                    if (not {"name", "template"} <= set(body) or set(body) - {"name", "template", "size", "resources"}
                            or ("size" in body and "resources" in body)
                            or not isinstance(body["name"], str) or not isinstance(body["template"], str)
                            or ("size" in body and not isinstance(body["size"], str))
                            or ("resources" in body and not isinstance(body["resources"], dict))):
                        raise ValueError("Provide name, template and either a size preset or custom resources.")
                    job = fleet.submit(create[1], "allocate" if create[2] == "allocations" else "create", **body)
                elif install:
                    if body:
                        raise ValueError("Template installation accepts an empty object; resources come from its manifest.")
                    job = fleet.submit(install[1], "install_template", template=install[2])
                elif action:
                    if set(body) != {"action"} or body["action"] not in {"pause", "resume", "hibernate", "stop"}:
                        raise ValueError("Choose pause, resume, hibernate or stop.")
                    job = fleet.submit(action[1], body["action"], name=action[2])
                else:
                    return self.respond(404, {"error": "Unknown endpoint."})
                self.respond(202, {"job": job})
            except (ValueError, TypeError, KeyError) as exc:
                self.respond(400, {"error": str(exc)})
            except Exception:
                self.respond(500, {"error": "Fleet request failed; inspect service diagnostics."})

    return Handler


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--state-dir", type=Path, default=Path.home() / ".local/state/enginebox/jobs")
    args = parser.parse_args(argv)
    fleet = server = None
    try:
        if not 1 <= args.port <= 65535:
            raise ValueError("--port must be between 1 and 65535.")
        token = api_token()
        fleet = Fleet(load_hosts(args.config), state_dir=args.state_dir)
        server = ThreadingHTTPServer(("127.0.0.1", args.port), handler_for(fleet, token))
        def stop(_signal, _frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, stop)
        fleet.warm.start()
        fleet.template_sync.start()
        fleet.agent_sync.start()
        print(f"Engine Box API listening on http://127.0.0.1:{server.server_port}", flush=True)
        server.serve_forever()
    except KeyboardInterrupt:
        return
    except (OSError, ValueError) as exc:
        parser.exit(1, f"Engine Box API: {exc}\n")
    finally:
        if server:
            server.server_close()
        if fleet:
            # Graceful shutdown lets admitted jobs finish. Abrupt termination is
            # recovered as failed/inspect-required from the persisted journal.
            fleet.warm.stop()
            fleet.template_sync.stop()
            fleet.agent_sync.stop()
            fleet.accounts.close()
            if fleet.agents:
                fleet.agents.close()
            fleet.executor.shutdown(wait=True)


if __name__ == "__main__":
    main()
