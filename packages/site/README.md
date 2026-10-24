# Engine Box site

The product landing page and developer guide. Static HTML, CSS and browser JavaScript; no controller connection or account data is required.

From the repository root:

```sh
pnpm install --frozen-lockfile
pnpm dev:site
```

Open `http://127.0.0.1:4178`. Source changes require a rebuild; the preview server serves `dist/`.

```sh
pnpm --filter enginebox-site check
pnpm --filter enginebox-site test
pnpm --filter enginebox-site build
pnpm --filter enginebox-site test:e2e
```

`build.mjs` generates the landing page, documentation routes, search index and a 404 page into `dist/`. Deploy that directory to a static host with directory-index routing and `404.html` support. Root-relative URLs assume the site is served at `/`.

Edit the landing page in `src/home.mjs`, the pricing page in `src/pricing.mjs`, guides in `src/guides.mjs`, and the shared layout in `build.mjs`. Styles and browser behavior live in `public/`. Documentation groups and navigation are generated from the guide definitions. The build validates guide identifiers and internal documentation routes.

The interactive environment diagram is a browser-only concept demo. Its private edits, pinned revisions and sleep/wake controls explain the planned persistent core workflow; they do not operate a real fleet. Keep planned capabilities clearly labeled and verify command examples against the runtime before publishing.

Browser tests cover desktop and mobile interactions, documentation search, copying commands, theme persistence, navigation and missing pages. They run the site in local Wrangler with a fixture dashboard service, including the dashboard entry link, JavaScript assets and API forwarding. No production controller connection is used. Tests use the installed Playwright Chromium browser.

## Pricing page and checkout

`/pricing/` is generated from `src/pricing.mjs` and included in the sitemap. Its license button is controlled by the build-time environment variable `CHECKOUT_URL`:

```sh
CHECKOUT_URL=https://example.com/checkout pnpm --filter enginebox-site build
```

- Set, it must be an `https://` URL; the plan shows a "Get a license" link to it.
- Unset, the plan shows a disabled "Checkout opening soon" button with no link, and the build prints a warning. No checkout exists yet.

`SITE_URL` sets the canonical origin (default `https://enginebox.coopcollective.dev`) and `OUT_DIR` overrides the `dist/` output directory (used by the build tests).

## Deploy

The site is a Cloudflare Worker with static assets at `https://enginebox.coopcollective.dev` (see `wrangler.jsonc`). **Get started**, **Start building**, and the **Dashboard** navigation link open `https://enginebox.coopcollective.dev/dashboard/`.

The `DASHBOARD` service binding connects to the existing `enginebox-dashboard-edge` Worker, which owns the private VPC connection to the native dashboard. `/dashboard/` maps to its root page; `/api/*`, `/_next/*`, and `/healthz` retain their paths. The public controller origin stays unchanged, including Windows node enrollment. The existing dashboard owns login, session cookies and node authentication; this site has no controller credential. Requests and responses stream through the fixed binding without following redirects.

Keep the Worker entrypoint, `run_worker_first`, and `DASHBOARD` binding when deploying. Replacing this with a static-only custom domain hides both the dashboard and node API. Provision the dashboard edge before deploying to another account. The static `dev:site` preview only serves the website; `test:e2e` exercises the combined routing with its local fixture.

With Wrangler signed in to the account:

```sh
pnpm --filter enginebox-site run deploy
```

This builds `dist/` and runs `wrangler deploy`, which also attaches the custom domain and service binding. Set `CHECKOUT_URL` in the same shell once license checkout opens. Verify the homepage and docs, then click through to `/dashboard/` and verify login and `/api/node/*` before considering a deployment healthy.
