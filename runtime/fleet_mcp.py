"""MCP fleet tools using the same authenticated API and admission controls as the UI."""
import json
import os
from typing import Literal
from urllib.error import HTTPError
from urllib.parse import urlparse
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener

from mcp.server import MCPServer

from enginebox import identifier
from fleet_api import api_token


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # urllib forwards Authorization on redirects by default. The configured
        # API origin is the only recipient authorized to receive this token.
        return None


def request(path, body=None):
    base = os.environ.get("ENGINEBOX_API_URL", "http://127.0.0.1:8765").rstrip("/")
    parsed = urlparse(base)
    if (parsed.scheme not in {"http", "https"} or parsed.username or parsed.password
            or parsed.path or parsed.query or parsed.fragment or not parsed.hostname):
        raise ValueError("ENGINEBOX_API_URL must be an HTTP(S) origin.")
    if parsed.scheme == "http" and parsed.hostname not in {"localhost", "127.0.0.1", "::1"}:
        raise ValueError("Use HTTPS for a remote API, or an SSH tunnel to loopback.")
    if parsed.port is not None and not 1 <= parsed.port <= 65535:
        raise ValueError("ENGINEBOX_API_URL has an invalid port.")
    data = None if body is None else json.dumps(body).encode()
    req = Request(base + path, data=data, headers={"Authorization": "Bearer " + api_token(),
                                                "Content-Type": "application/json"})
    try:
        # Direct API/SSH-tunnel access only; ambient proxy settings must not send
        # a loopback bearer token to an unrelated forward proxy.
        with build_opener(ProxyHandler({}), NoRedirect()).open(req, timeout=90) as response:
            return json.load(response)
    except HTTPError as exc:
        try:
            detail = json.load(exc).get("error", f"HTTP {exc.code}")
        except (ValueError, AttributeError):
            detail = f"HTTP {exc.code}"
        finally:
            exc.close()
        raise RuntimeError(detail) from None


def register(mcp):
    @mcp.tool()
    def fleet_inventory() -> dict:
        """List configured hosts, capacity budgets, prepared templates and VMs."""
        return request("/v1/fleet")

    @mcp.tool()
    def fleet_create(host_id: str, name: str, template: str, size: Literal["small", "standard", "large"] = "standard") -> dict:
        """Queue a clean VM clone and boot after checking resource reserves. Poll fleet_job for readiness."""
        identifier(host_id)
        identifier(name, instance=True)
        return request(f"/v1/hosts/{host_id}/instances", {"name": name, "template": template, "size": size})

    @mcp.tool()
    def fleet_allocate(host_id: str, name: str, template: str,
                       size: Literal["minimum", "recommended", "small", "standard", "large"] = "recommended",
                       resources: dict[str, int] | None = None) -> dict:
        """Assign a fresh warm VM or create a cold clone. Custom cpu/memory_gib/disk_gib must meet the template minimum.

        Poll fleet_job. Use result.name (the physical VM name) for later actions;
        workspace_name is its friendly label. An assigned VM is never recycled.
        """
        identifier(host_id)
        identifier(name, instance=True)
        selection = {"resources": resources} if resources is not None else {"size": size}
        return request(f"/v1/hosts/{host_id}/allocations", {"name": name, "template": template, **selection})

    @mcp.tool()
    def template_catalog() -> dict:
        """List portable template manifests, resource requirements and per-host installed revisions."""
        return request("/v1/templates")

    @mcp.tool()
    def template_register(manifest: dict | None = None, url: str | None = None) -> dict:
        """Register controller desired state from JSON or HTTPS; automatically synchronize every registered node."""
        if (manifest is None) == (url is None):
            raise ValueError("Provide exactly one manifest or URL.")
        return request("/v1/templates", {"manifest": manifest} if manifest is not None else {"url": url})

    @mcp.tool()
    def template_install(host_id: str, template: str) -> dict:
        """Legacy targeted retry; registered templates already synchronize automatically to every node."""
        identifier(host_id)
        identifier(template)
        return request(f"/v1/hosts/{host_id}/templates/{template}/install", {})

    @mcp.tool()
    def template_sync(template: str) -> dict:
        """Retry synchronization of a controller template to every registered node; poll template_catalog."""
        identifier(template)
        return request(f"/v1/templates/{template}/sync", {})

    @mcp.tool()
    def agent_connections() -> dict:
        """Show controller agent connection readiness without exposing credentials."""
        return request("/v1/agents")

    @mcp.tool()
    def fleet_action(host_id: str, name: str, action: Literal["pause", "resume", "hibernate", "stop"]) -> dict:
        """Queue a lifecycle change for a fleet-created VM. Stop discards process memory; hibernate preserves it."""
        identifier(host_id)
        identifier(name, instance=True)
        return request(f"/v1/hosts/{host_id}/instances/{name}/actions", {"action": action})

    @mcp.tool()
    def fleet_job(job_id: str) -> dict:
        """Poll queued/running/succeeded/failed status and timing for a fleet operation."""
        import re
        if not re.fullmatch(r"[a-f0-9]{32}", job_id):
            raise ValueError("Invalid job ID.")
        return request(f"/v1/jobs/{job_id}")


def main():
    if os.environ.get("ENGINEBOX_GUEST", "0") != "0":
        raise ValueError("Fleet MCP is an operator tool; do not install operator credentials in guests.")
    mcp = MCPServer("Engine Box Fleet", instructions="Manage prepared VMs through the capacity-aware fleet API.")
    register(mcp)
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
