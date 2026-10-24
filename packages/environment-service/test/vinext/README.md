# Vinext stable-dev fixture

`pnpm dev` runs `vinext dev`. Vite handles React HMR, while the Cloudflare Vite
plugin runs the server in workerd with local KV. This project never deploys a
Worker, uses no Cloudflare credentials and disables remote bindings.

Use the standalone lockfile: `pnpm install --frozen-lockfile`. The environment
image needs Node, pnpm (or Corepack), and system CA certificates. Configure
`updateMode: "files"`, container port 8787, health `/health`, and writable cache
directories `.wrangler`, `.vinext`, `.next`, `node_modules/.mf` and
`node_modules/.vite-temp`. Vite's optimization cache uses `/tmp`; simulated KV
uses `ENGINEBOX_DATA_DIR` (`/data` in Engine Box). Keep dependency and package-manager
caches inside the prepared workspace if the image doesn't provide them.

Set `DEV_PUBLIC_HOST` to the exact approved public hostname. The project's Vite
config allows that host and advertises HTTPS WebSocket port 443 through the
existing proxy. For local development, leave it unset. The supervisor only runs
the configured command, updates files, and forwards a port.

Start `../live-vinext-browser.mjs URL BEFORE_VERSION AFTER_VERSION OUTPUT` with
`ENGINEBOX_LIVE_VINEXT=1`. Wait for `OUTPUT.ready`, then edit `app/release.ts` in a
new Git-free LayerFS branch, checkpoint/close it and promote against observed main.
The probe requires public WebSocket Upgrade, a framework HMR update, changed page
version with no document navigation and persisted KV data. It separately records
whether the framework preserves or resets component-local React state; remounts
are framework behavior, not evidence of an environment process restart.
Independently probe HTTP and compare dev process/container
identities across the promotion. Use only a disposable project; real projects need
an appropriate access policy for their dev server and local simulator.
