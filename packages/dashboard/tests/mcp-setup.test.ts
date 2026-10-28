import { test } from "node:test";
import assert from "node:assert/strict";
import { mcpConfiguration } from "../lib/mcp-setup";
test("MCP setup config uses stdio and a token file placeholder without exposing a token or inventing remote transport", () => {
  const server = JSON.parse(mcpConfiguration).mcpServers.enginebox;
  assert.equal(server.command, "enginebox-fleet-mcp");
  assert.equal(server.env.ENGINEBOX_API_URL, "http://127.0.0.1:8765");
  assert.equal(server.env.ENGINEBOX_API_TOKEN_FILE, "/absolute/path/to/private/controller.token");
  assert.equal(server.env.ENGINEBOX_API_TOKEN, undefined);
  assert.equal(server.url, undefined);
});
