# Wrangler file-watcher fixture

This standalone project is intended for a disposable LayerFS import. Its own npm
lock pins Wrangler 4.152.0. It runs local simulated KV, requires no Cloudflare
credential and performs no deployment. `/health` and `/api/state` report source
version and persistent identity/count; POST `/counter` saves an interaction.

Provision `updateMode: "files"`, a digest-pinned Node 24 image with system CA
certificates, `writableDirs: [".wrangler", "node_modules/.mf"]`,
`prepare: ["npm", "ci", "--cache", "/tmp/npm-cache"]` and
`command: ["npm", "run", "dev"]`. Forward container port 8787 and retain `/data`.
The application's dev script is ordinary Wrangler; the supervisor knows nothing
about its reload protocol. Never expose a real project's local simulator publicly
without choosing its access policy.

Open the fixture using `../live-watch-browser.mjs` with explicit opt-in. Wait for
its `.ready` report, then fork a LayerFS workspace, edit only `release.mjs`, verify
the version and JavaScript syntax, checkpoint, close and promote against the exact
observed main revision. The browser must receive a reload frame and navigate to the
new version without the test calling reload. Compare process/container identities
and continuously probe HTTP around promotion separately.

Wrangler's `/cdn-cgi/live-reload` path cannot pass through the public Cloudflare
Worker edge because Cloudflare reserves that prefix. Test native reload over a
direct port/SSH forward, or provision an ingress that preserves the path. Wrangler
also validates the Host/Origin of its internal endpoints; configure a public dev
server's allowed host using the project's supported settings. Ordinary application
HTTP updates through the existing edge independently of browser reload.
