/** Local stdio setup only. The public dashboard is not a remote MCP endpoint. */
export const mcpConfiguration = JSON.stringify({
  mcpServers: {
    enginebox: {
      command: "enginebox-fleet-mcp",
      env: {
        ENGINEBOX_API_URL: "http://127.0.0.1:8765",
        ENGINEBOX_API_TOKEN_FILE: "/absolute/path/to/private/controller.token",
      },
    },
  },
}, null, 2);
