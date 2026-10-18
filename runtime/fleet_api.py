"""Loopback, bearer-authenticated fleet API for the dashboard and MCP client."""
from __future__ import annotations

import argparse
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import sys

from fleet import Fleet, load_hosts


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
                if not 1 <= length <= 16384:
                    return self.respond(413, {"error": "Request must contain 1–16384 bytes."})
                body = json.loads(self.rfile.read(length))
                if not isinstance(body, dict):
                    raise ValueError("Body must be an object.")
                create = re.fullmatch(r"/v1/hosts/([a-z0-9][a-z0-9-]{0,48})/instances", self.path)
                action = re.fullmatch(r"/v1/hosts/([a-z0-9][a-z0-9-]{0,48})/instances/(ebox-[a-z0-9][a-z0-9-]{0,48})/actions", self.path)
                if create:
                    if set(body) != {"name", "template", "size"} or not all(isinstance(v, str) for v in body.values()):
                        raise ValueError("Provide string fields name, template and size.")
                    job = fleet.submit(create[1], "create", **body)
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
            fleet.executor.shutdown(wait=True)


if __name__ == "__main__":
    main()
