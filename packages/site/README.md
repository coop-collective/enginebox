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

Browser tests cover desktop and mobile interactions, documentation search, copying commands, theme persistence, navigation and missing pages. Tests use the installed Playwright Chromium browser.

## Pricing page and checkout

`/pricing/` is generated from `src/pricing.mjs` and included in the sitemap. Its license button is controlled by the build-time environment variable `CHECKOUT_URL`:

```sh
CHECKOUT_URL=https://example.com/checkout pnpm --filter enginebox-site build
```

- Set, it must be an `https://` URL; the plan shows a "Get a license" link to it.
- Unset, the plan shows a disabled "Checkout opening soon" button with no link, and the build prints a warning. No checkout exists yet.

`SITE_URL` sets the canonical origin (default `https://enginebox.dev`) and `OUT_DIR` overrides the `dist/` output directory (used by the build tests).
