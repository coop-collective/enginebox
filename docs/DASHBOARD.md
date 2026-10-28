# Fleet dashboard

The optional dashboard is a Vinext App Router application targeting Node.js. It talks to the fleet REST API through server-side route handlers. The Python runtime remains independent of the dashboard, React and Node.

## What is implemented

- Sidebar navigation for Compute, Workspaces, Agents, Templates, Environments and MCP. Workspaces consolidates hosted/GitHub sources and source/storage controls with project activity and measured preparation. MCP teaches local stdio setup and retains OAuth app grant management. Each opens its own view; the navigation becomes a compact row on mobile. Switching views keeps recent operations in memory and continues polling active infrastructure jobs.
- **Compute** combines configured VM hosts and enrolled Windows workers in one machine list. Pending and revoked registrations stay collapsed. Add machine issues the Windows installer command; CPU/memory/disk allowances are selected on that PC. The online count includes both inventories; VM resource cards and VM allocation controls use only VM hosts. Worker allowances are not physical host utilization or additional VM capacity. Either inventory remains visible if the other fails, with a visible error.
- Live fleet inventory, online/offline hosts, reserved and allocated CPU/memory, free disk, available memory, and capacity by workspace size. The top CPU card shows allocated cores and percentage, not CPU utilization. Host memory usage is total minus available memory, clamped to the reported total.
- Workspace names, host, state, IP addresses, allocated resources, and current memory usage when the host reports it.
- Allocating a workspace from a matching ready, paused reserve, with an explicit cold-start fallback. Completed operations show the actual startup mode and ready time reported by the API. Availability is advisory; another request can claim a ready instance before submission.
- Template minimum/recommended presets and custom CPU, RAM and disk allocations. Custom values must be whole numbers at or above the template minimum. Older templates without resource metadata retain the legacy named sizes.
- A template catalog panel for registering JSON manifests or HTTPS URLs, exporting portable JSON, automatic synchronization to every registered node, and per-node readiness and errors.
- Pause, resume, hibernate and stop controls for assigned managed workspaces. Unmanaged bases and preparing, ready or quarantined pool instances have no lifecycle controls. Each pending operation affects only its target row. Stop asks for confirmation because it ends running processes.
- An Agents panel for connecting Codex (ChatGPT), Claude and Cursor once through controller-level native sign-in sessions, with separate workspace-delegation readiness. It displays status, official provider links, device codes, Claude authorization-code entry and cancellation.
- Usage cards separate provider allowance windows, quota resets, optional credits and billing renewal. Codex uses its native read-only quota interface; Claude/Cursor and unreported billing dates display explicit unavailable states with provider links.
- Asynchronous operation status, error messages, loading states, empty fleet instructions and responsive mobile layouts.
- A separate operator sign-in, an eight-hour signed HttpOnly session, and exact Origin checks for login, logout and mutations.

The page reads `/v1/fleet` and `/v1/contributors` every ten seconds while signed in and visible. Refresh updates both inventories. Registration changes refresh the worker list immediately. Pending jobs are polled every 1.5 seconds; completed jobs trigger a fresh inventory read. Recent operations are kept in the current page's memory and are cleared on reload or sign-out. Infrastructure jobs cannot be cancelled through the dashboard. There is no multi-user account system.

The assigned-workspace count excludes unmanaged bases and pool reserves. The instance inventory still shows all of them, with reserve state clearly marked. Claimed reserves keep their Incus VM ID; the requested workspace name appears above that ID, and lifecycle requests always use the actual VM ID. Each VM host’s **Details** shows its configured reserve's ready/preparing/target counts and refill errors. Paused reserves retain their memory allocation.

Warm matching compares template and resolved CPU/RAM/disk requirements, so a custom allocation can match a reserve prepared under a named preset. Sizing prefers the selected host's `host_resources` requirements when present. Cold capacity uses the API's available CPU, memory, disk and instance-count budgets. Disk admission includes the requested root disk plus RAM size and 2 GiB of checkpoint overhead. The backend enforces all limits and readiness checks.

The controller stores the canonical template. Registration automatically queues installation or synchronization on all registered nodes, including nodes added later. The catalog has no target-host selector: it shows the desired revision and each node's pending, syncing, ready or failed state, with capacity and connection errors kept visible. **Retry sync** requests a fleet-wide retry. The open catalog polls every two seconds and stops polling when closed. Only templates successfully installed on a host are offered in its workspace form. The backend validates manifests, supported recipes and remote imports; templates do not carry provider credentials.

Each catalog record's **Export JSON** button downloads `{id}-r{revision}.json` with only the supported v1 manifest fields. Host inventory, deployment receipts, snapshot references and catalog digests are excluded. The exported file can be registered in another Engine Box installation through JSON import or an HTTPS URL.

The app has no demo mode and no production fallback to synthetic inventory. Test fixtures live only in `packages/dashboard/tests/fixtures/`.

## Agents and native accounts

Open **Agents**, or choose **Manage agents** from an assigned workspace. **Connect** starts the provider's controller-level native sign-in flow. There is no host or workspace selector, and opening the panel never resumes or changes a VM. Agent connections remain accessible even before the fleet has a workspace.

Complete sign-in on the official provider page. Codex may show a device code to enter there. Claude may request the authorization code returned by its browser flow; the dashboard accepts it only while that native session requests input, masks it and clears the field on submission. Cursor completes in the browser. The panel never asks for a provider password or API key. **Cancel sign-in** stops the pending native flow; **Reconnect** starts a new one. Neither action is a provider logout.

The controller manages the owner's provider connections; the backend reports whether access is ready for private workspaces across the fleet. The UI shows **Signed in** separately from delegation readiness: unsupported providers, connections requiring reauthentication and unconfirmed access are explicit. Available-workspace counts appear only when the API confirms readiness. Provider details explain limitations, and unavailable controller clients cannot start a login. Sign-in status does not establish paid-plan eligibility, quota or remaining subscription balance. **Refresh agents** reads the current status. Credentials never become part of portable template exports or shared prepared bases.

Active sign-ins are polled approximately once per second. Closing the panel, signing out or leaving the page stops that view's polling and discards its local fields. Closing the view does not cancel the native flow; reopening recovers a still-active session from the API. URLs and codes are kept only in page memory, never browser storage. Only the verified HTTPS authorization endpoints for `auth.openai.com`, `claude.com` and `cursor.com` are clickable; callback URLs, token-bearing parameters and unexpected query fields are rejected.

The usage cards read the connected controller account, not a per-workspace copy of its allowance. A passed quota reset does not imply that the billing cycle renewed or that an account has a full allowance. Stale values are labeled, and no model prompt is sent merely to populate usage. See [usage collection and limitations](AGENT-USAGE.md).

## Local setup

Use Node 22.12 or later. From the repository root:

```sh
cd packages/dashboard
pnpm install --frozen-lockfile
pnpm build
```

Create `packages/dashboard/.env.local` with these values. It is ignored by Git. Keep permissions restricted to the service owner (`chmod 600 .env.local`). Use distinct random values for the password, session secret and fleet API token.

```dotenv
HOST=127.0.0.1
PORT=3000
ENGINEBOX_DASHBOARD_ORIGIN=http://127.0.0.1:3000
ENGINEBOX_API_URL=http://127.0.0.1:8765
ENGINEBOX_API_TOKEN=<the fleet service's existing bearer token, at least 32 characters>
ENGINEBOX_DASHBOARD_PASSWORD=<a separate random operator password, at least 16 characters>
ENGINEBOX_SESSION_SECRET=<a separate random signing secret, at least 32 characters>
```

To let other apps connect over OAuth, also set `ENGINEBOX_OAUTH_STATE_DIR` to an absolute private directory (see [OAuth](OAUTH.md)); without it the OAuth endpoints answer 404. The placeholders above must be replaced; they are not usable credentials. `openssl rand -hex 32` generates a suitable random value. The API token must match the configured fleet service. The operator types only `ENGINEBOX_DASHBOARD_PASSWORD` into the login page; the browser never receives the API token or signing secret.

```sh
pnpm start
```

Open exactly `http://127.0.0.1:3000`; `localhost` is a different origin. For a different port, change both `PORT` and `ENGINEBOX_DASHBOARD_ORIGIN`. `pnpm dev` starts the development server on loopback port 3000 and also reads `.env.local`.

For a service running on a dedicated server, keep it on loopback and use an SSH tunnel:

```sh
ssh -N -L 127.0.0.1:3000:127.0.0.1:3000 your-server
```

Build output is in `packages/dashboard/dist/`; `pnpm start` runs the tested Vinext production server with explicit bind settings. Vinext also generates `dist/standalone/`, but its generated server entry has different defaults. Use the repository's start script rather than launching an entry that binds all interfaces by default.

## Managed service and updates

Use the native [installer](INSTALL.md) and [supervisor](UPDATES.md). The supervisor keeps stable loopback API/dashboard addresses, stages a candidate controller in standby, drains admitted work and transfers exclusive state ownership before switching traffic. Keep the dashboard password and session secret stable across releases to preserve existing sessions. The first move from Python v0.2 requires a short control-plane interruption; VM workloads continue independently.

The installer provides service plans for systemd, launchd and Windows Task Scheduler. A native package does not qualify that platform for Incus compute or native provider authentication. Private configuration/state stays outside immutable release directories. Production services need the built dashboard and Node 22.12 or newer; building from source uses the pinned root pnpm workspace.

## Access and credentials

`ENGINEBOX_DASHBOARD_ORIGIN` must be an exact origin with no path or trailing slash. The proxy does not trust forwarded host/protocol headers to decide which origins to permit. All state-changing requests require a matching `Origin` header, including login and logout.

Cookies use `HttpOnly`, `SameSite=Strict`, an eight-hour expiry and `Secure` when the configured origin uses HTTPS. Signing also includes the operator password, so rotating either the password or session secret invalidates existing sessions. Logout clears the browser cookie; individual stolen cookies cannot be revoked without rotating credentials. A process-local limit allows at most twenty login attempts per five minutes.

For access through a TLS reverse proxy, set an HTTPS dashboard origin. The start script refuses non-loopback binding unless that HTTPS origin is set. TLS must be provided by the reverse proxy, and direct access to the plain HTTP upstream must be restricted by the operator. The dashboard does not enable TLS itself. Plain HTTP fleet API URLs are accepted only for loopback; remote API connections require HTTPS.

Only the configured fleet endpoints are proxied. Browser-supplied Authorization headers are not forwarded. The server uses the configured API token, rejects upstream redirects, bounds ordinary JSON request bodies to 8 KiB and template-registration bodies to 1 MiB, and disables response caching. This is a single-operator control plane, not tenant authorization.

The dashboard does not embed noVNC, open desktop ports or proxy model inference. Lifecycle buttons call the environment API; account controls coordinate the official native harnesses' own authentication. In particular, publishing the dashboard does not make an unauthenticated desktop listener safe.

## API contract

| Browser operation | Fleet endpoint |
| --- | --- |
| Read capacity, templates, sizes and instances | `GET /v1/fleet` |
| Allocate a workspace | `POST /v1/hosts/{id}/allocations` with `{name, template, size}` or `{name, template, resources: {cpu, memory_gib, disk_gib}}` |
| Explicit cold creation (API compatibility) | `POST /v1/hosts/{id}/instances` with `{name, template, size}` |
| Read template catalog and deployment receipts | `GET /v1/templates` |
| Register a template | `POST /v1/templates` with `{manifest}` or `{url}` |
| Retry automatic template synchronization across nodes | `POST /v1/templates/{template_id}/sync` with `{}` |
| Change workspace state | `POST /v1/hosts/{id}/instances/{name}/actions` with `{action}` |
| Poll operation state | `GET /v1/jobs/{id}` |
| Read native account status and active sign-ins | `GET /v1/agents` |
| Start native sign-in | `POST /v1/agents/{provider}/login` with `{}` |
| Poll native sign-in | `GET /v1/agents/{provider}/sessions/{session_id}` |
| Submit a requested Claude authorization code | `POST /v1/agents/claude/sessions/{session_id}/input` with `{text}` |
| Cancel native sign-in | `POST /v1/agents/{provider}/sessions/{session_id}/cancel` with `{}` |

Workspace mutations return HTTP 202 with `{job: {id, status}}`; template registration returns synchronously and starts automatic node synchronization. Global sync retries return an acceptance response; the catalog's `sync` object is authoritative for node progress. It contains `desired_revision` and `nodes` with host ID, label, status and optional error/job ID. Job status is `queued`, `running`, `succeeded` or `failed`. Allocation results contain the actual `name`, requested `workspace_name`, `startup` (`warm` or `cold`) and `ready_seconds`. The frontend treats failed inventory and job requests as visible errors. It does not infer success from a timeout. Capacity is advisory in the UI; the fleet service must enforce it and reject conflicting concurrent mutations.

Native sign-in endpoints return `{session}` independently of infrastructure jobs. Providers are `codex`, `claude` and `cursor`; session IDs are 32 lowercase hexadecimal characters. Status is `starting`, `waiting`, `succeeded`, `failed`, `cancelled` or `expired`. Provider status includes optional `delegation_status` (`ready`, `unsupported`, `reauth_required`, `unknown`) and `workspace_count`; sign-in alone does not establish delegation readiness. The proxy permits only these global agent routes, with the ordinary 8 KiB body limit. Claude authorization codes are limited to 8–2048 characters and validated by the backend.

## Validation

```sh
cd packages/dashboard
pnpm typecheck
pnpm test
pnpm build
pnpm exec playwright install chromium
pnpm test:e2e
```

The Node tests cover signed sessions, expiry, credential rotation, exact origins, proxy authentication, path restrictions, backend failure handling, body limits, custom resource validation, cold capacity, warm matching and official provider URL validation. Playwright runs the production build with a test-only API on `127.0.0.1:18765` and dashboard on `127.0.0.1:3007`. It exercises login/logout, token separation, row-isolated lifecycle operations, resource-card math, reserve restrictions, warm/cold allocation, custom minimums, canonical template import and automatic revision/new-node sync, node capacity failures and retry, async job failures, empty/error states and mobile layout. Account tests cover device flow, code entry, reconnect/cancel, completion, global connections without a running workspace, delegation limitations, session recovery, polling cleanup and unauthenticated requests. It never calls a real fleet API, signs in to a real provider or stops a developer workspace.

Tested initial dependency versions: Vinext 1.2.1, React 19.3.1, Vite 8.3.4. Exact versions and transitive integrity hashes are in the root `pnpm-lock.yaml`. The scaffold was generated with the official `create-vinext-app --platform=node` workflow; see the [upstream setup and Node hosting docs](https://github.com/cloudflare/vinext#starting-a-new-vinext-project).

The v0.2 dependency audit reported six high-severity dependency entries from the `braces` 3.0.3 glob-tooling chain. They trace to [GHSA-vfj7-8cjw-p6xm](https://github.com/advisories/GHSA-vfj7-8cjw-p6xm); 3.0.3 was the version inspected at the time. The dashboard does not pass user input to these glob APIs. The `fflate` advisory was addressed with a compatible 0.7.5 override. Keep the remaining advisory visible during dependency upgrades; do not force-downgrade Vinext to satisfy the audit tool's suggested major-version replacement.

## GitHub repositories and Actions

The **GitHub** panel connects the repository App, lists accessible installations and private repositories, selects a branch and opens the existing Storage review/import/accepted-main export flow with that selection. The controller delegates repository-scoped credentials for each network operation. Runner administration is a separate App connection with organization approval. Pool status and draining are visible alongside runner job receipts. See [GitHub Actions setup and scope](GITHUB-ACTIONS.md), including live qualification and runner-pool distribution limits.

Browser checks can use `ENGINEBOX_TEST_FLEET_PORT` and `ENGINEBOX_TEST_DASHBOARD_PORT` to avoid existing local services. GitHub feature tests use signed fixture sessions, as storage tests do, so they do not weaken or exhaust the production login limit.
