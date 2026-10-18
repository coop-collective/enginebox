"""Local stdio MCP tools; SSH authenticates operators, guest mode only controls its desktop."""
from typing import Literal

from mcp.server import MCPServer
from mcp.server.mcpserver import Image

from enginebox import EngineBox, computer, guest_mode, screenshot

mcp = MCPServer("Engine Box", instructions="Native agent CLIs run unchanged. These tools control environments and the desktop.")

if not guest_mode():
    @mcp.tool()
    def environments() -> list[dict]:
        """List Engine Box VMs on the configured host."""
        return EngineBox().list()

    @mcp.tool()
    def environment_state(name: str, action: Literal["pause", "resume", "hibernate", "stop"]) -> dict:
        """Pause retains RAM. Hibernate saves RAM to disk. Stop discards running process state."""
        return EngineBox().lifecycle(name, action)

    @mcp.tool()
    def environment_exec(name: str, argv: list[str], timeout: int = 60) -> str:
        """Execute argv as the VM's agent user. Use the native CLI directly, or tmux for persistent sessions."""
        return EngineBox().execute(name, argv, timeout).decode(errors="replace")

    @mcp.tool()
    def environment_snapshot(name: str, snapshot: str) -> str:
        """Snapshot private disk and, if running, RAM. Shared network mounts are excluded."""
        return EngineBox().snapshot(name, snapshot)

    @mcp.tool()
    def environment_fork(name: str, snapshot: str, target: str) -> dict:
        """Create a stopped private disk clone; includes any credentials stored in the source snapshot."""
        return EngineBox().fork(name, snapshot, target)

    @mcp.tool()
    def environment_restore(name: str, snapshot: str, stateful: bool = True) -> dict:
        """Replace a stopped VM's disk with a snapshot, optionally restoring RAM. Later writes are lost."""
        return EngineBox().restore(name, snapshot, stateful)


@mcp.tool()
def desktop_screenshot(name: str = "ebox-dev") -> Image:
    """Capture the virtual desktop as a PNG image."""
    return Image(data=screenshot(name), format="png")


@mcp.tool()
def desktop_click(x: int, y: int, button: Literal[1, 2, 3] = 1, name: str = "ebox-dev") -> str:
    """Click a coordinate on the 1440 by 900 desktop."""
    if not (0 <= x < 1440 and 0 <= y < 900):
        raise ValueError("Coordinates are outside the desktop.")
    computer(["xdotool", "mousemove", "--sync", str(x), str(y), "click", str(button)], name)
    return "clicked"


@mcp.tool()
def desktop_type(text: str, name: str = "ebox-dev") -> str:
    """Type literal text into the focused application."""
    if len(text) > 10000:
        raise ValueError("Text exceeds 10,000 characters.")
    computer(["xdotool", "type", "--clearmodifiers", "--", text], name)
    return "typed"


@mcp.tool()
def desktop_key(key: str, name: str = "ebox-dev") -> str:
    """Send an X11 key or chord, for example Return, Escape, ctrl+l."""
    if not key or key.startswith("-") or len(key) > 100:
        raise ValueError("Invalid key chord.")
    computer(["xdotool", "key", "--clearmodifiers", key], name)
    return "pressed"


def main():
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
