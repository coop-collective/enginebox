# Storage qualification

The current architecture uses LayerFS workspaces, a Cloudflare controller, R2 durable object storage, and optional node caches. See [local fleet development](../../docs/LOCAL-FLEET.md) for the supported implementation and qualification boundary.

Run `pnpm test:fleet` for integrity and cache regression checks and `pnpm fleet:smoke` for the isolated multi-container distribution test. Product tests live in `packages/`; future standalone performance campaigns belong here. Keep project contents and raw reports in ignored private state.
