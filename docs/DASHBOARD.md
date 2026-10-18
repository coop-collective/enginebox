# Fleet dashboard

The optional dashboard is a Vinext App Router application targeting Node.js. It talks to the fleet REST API through server-side route handlers. The Python runtime remains independent of the dashboard, React and Node.

## What is implemented

- Live fleet inventory, online/offline hosts, reserved and allocated CPU/memory, free disk, available memory, and capacity by workspace size.
- Workspace names, host, state, IP addresses, allocated resources, and current memory usage when the host reports it.
- Creating a workspace from a configured prepared template and named size.
- Pause, resume, hibernate and stop controls for managed workspaces. Unmanaged instances and prepared bases are read-only. Stop asks for confirmation because it ends running processes.
- Asynchronous operation status, error messages, loading states, empty fleet instructions and responsive mobile layouts.
- A separate operator sign-in, an eight-hour signed HttpOnly session, and exact Origin checks for login, logout and mutations.

The page reads `/v1/fleet` every ten seconds while signed in. Pending jobs are polled every 1.5 seconds; completed jobs trigger a fresh inventory read. Recent operations are kept in the current page's memory and are cleared on reload or sign-out. There is no cancellation button or multi-user account system.

The app has no demo mode and no production fallback to synthetic inventory. Test fixtures live only in `dashboard/tests/fixtures/`.

## Local setup

Use Node 22.12 or later. From the repository root:

```sh
cd dashboard
npm ci
npm run build
```

Create `dashboard/.env.local` with these values. It is ignored by Git. Keep permissions restricted to the service owner (`chmod 600 .env.local`). Use distinct random values for the password, session secret and fleet API token.

```dotenv
HOST=127.0.0.1
PORT=3000
ENGINEBOX_DASHBOARD_ORIGIN=http://127.0.0.1:3000
ENGINEBOX_API_URL=http://127.0.0.1:8765
ENGINEBOX_API_TOKEN=<the fleet service's existing bearer token, at least 32 characters>
ENGINEBOX_DASHBOARD_PASSWORD=<a separate random operator password, at least 16 characters>
ENGINEBOX_SESSION_SECRET=<a separate random signing secret, at least 32 characters>
```

The placeholders above must be replaced; they are not usable credentials. `openssl rand -hex 32` generates a suitable random value. The API token must match the configured fleet service. The operator types only `ENGINEBOX_DASHBOARD_PASSWORD` into the login page; the browser never receives the API token or signing secret.

```sh
npm start
```

Open exactly `http://127.0.0.1:3000`; `localhost` is a different origin. For a different port, change both `PORT` and `ENGINEBOX_DASHBOARD_ORIGIN`. `npm run dev` starts the development server on loopback port 3000 and also reads `.env.local`.

For a service running on a dedicated server, keep it on loopback and use an SSH tunnel:

```sh
ssh -N -L 127.0.0.1:3000:127.0.0.1:3000 your-server
```

Build output is in `dashboard/dist/`; `npm start` runs the tested Vinext production server with explicit bind settings. Vinext also generates `dist/standalone/`, but its generated server entry has different defaults. Use the repository's start script rather than launching an entry that binds all interfaces by default.

## Install as Linux services

The optional `scripts/install-control-plane.sh` installs a built checkout onto an existing systemd server. Build the dashboard on the target OS/architecture first; the installer uses its standalone output and does not install or replace Node. Python 3.11+, working `venv` support, Node 22.12+, systemd and network access for Python package installation must already exist.

Supply an existing non-root service user with the SSH/Incus permissions the fleet needs. The installer does not create users, SSH keys, sudoers rules or firewall rules. Use a private fleet TOML and an environment file containing the values above, both regular files with permissions `0600` or stricter.

```sh
sudo scripts/install-control-plane.sh \
  --config /private/path/fleet.toml \
  --env-file /private/path/control-plane.env \
  --user your-service-user
```

This is a read-only preflight and plan. It checks the source/build artifacts, input permissions, config schema, existing software, port conflicts and ownership markers. Apply the same plan explicitly:

```sh
sudo scripts/install-control-plane.sh \
  --config /private/path/fleet.toml \
  --env-file /private/path/control-plane.env \
  --user your-service-user --apply
```

The default prefix is `/opt/enginebox-control`; `--prefix` selects another unused or previously owned prefix, and `--api-port` changes the loopback API port. The dashboard port comes from `PORT` in the input environment file. Both services always bind `127.0.0.1` in this installer. Paths must not contain whitespace or systemd specifiers.

Each install writes a versioned release, creates a Python venv from the hashed dependency lock, installs the runtime package, copies the built standalone dashboard, and stores separate owner-only environment files for the two services. It does not expose the dashboard password/session secret to the API process. It installs only `enginebox-fleet-api.service` and `enginebox-dashboard.service`, refuses unknown existing units or unmarked installation directories, then enables/restarts those two services. The start wrapper is copied alongside the standalone build so the tested explicit bind behavior is preserved.

The API service is limited to 512 MiB memory and two CPU cores of processing time; the dashboard to 1 GiB and two cores. These limits apply to the control-plane processes, not to Incus VM allocations or unrelated host services.

```sh
systemctl status enginebox-fleet-api enginebox-dashboard
curl --fail http://127.0.0.1:8765/healthz
curl --fail http://127.0.0.1:3000/healthz
```

Private configuration and credentials are copied into each release; old releases are retained, so remove superseded releases deliberately when their rollback value ends. Job state lives separately under the prefix's `state/`. On upgrade, `previous` retains the prior release target. Roll back by stopping the two owned services, atomically replacing `current` with a symlink to the `previous` target, and restarting the services. If the service user or Node executable path changed, restore the matching unit configuration as well. The installer never deletes compute instances or previous releases. Service removal is explicit: stop/disable the two owned services and remove only their marked unit files; the installation prefix remains available for recovery.

## Access and credentials

`ENGINEBOX_DASHBOARD_ORIGIN` must be an exact origin with no path or trailing slash. The proxy does not trust forwarded host/protocol headers to decide which origins to permit. All state-changing requests require a matching `Origin` header, including login and logout.

Cookies use `HttpOnly`, `SameSite=Strict`, an eight-hour expiry and `Secure` when the configured origin uses HTTPS. Signing also includes the operator password, so rotating either the password or session secret invalidates existing sessions. Logout clears the browser cookie; individual stolen cookies cannot be revoked without rotating credentials. A process-local limit allows at most twenty login attempts per five minutes.

For access through a TLS reverse proxy, set an HTTPS dashboard origin. The start script refuses non-loopback binding unless that HTTPS origin is set. TLS must be provided by the reverse proxy, and direct access to the plain HTTP upstream must be restricted by the operator. The dashboard does not enable TLS itself. Plain HTTP fleet API URLs are accepted only for loopback; remote API connections require HTTPS.

Only the configured fleet endpoints are proxied. Browser-supplied Authorization headers are not forwarded. The server uses the configured API token, rejects upstream redirects, bounds JSON request bodies and disables response caching. This is a single-operator control plane, not tenant authorization.

The dashboard does not embed noVNC, open desktop ports, install agents or proxy provider authentication. Lifecycle buttons call the environment API; official native harnesses retain their own authentication. In particular, publishing the dashboard does not make an unauthenticated desktop listener safe.

## API contract

| Browser operation | Fleet endpoint |
| --- | --- |
| Read capacity, templates, sizes and instances | `GET /v1/fleet` |
| Create a workspace | `POST /v1/hosts/{id}/instances` with `{name, template, size}` |
| Change workspace state | `POST /v1/hosts/{id}/instances/{name}/actions` with `{action}` |
| Poll operation state | `GET /v1/jobs/{id}` |

Mutation responses are HTTP 202 with `{job: {id, status}}`. Job status is `queued`, `running`, `succeeded` or `failed`. The frontend treats failed inventory and job requests as visible errors. It does not infer success from a timeout. Capacity is advisory in the UI; the fleet service must enforce it and reject conflicting concurrent mutations.

## Validation

```sh
cd dashboard
npm run typecheck
npm test
npm run build
npx playwright install chromium
npm run test:e2e
```

The Node tests cover signed sessions, expiry, credential rotation, exact origins, proxy authentication, path restrictions, backend failure handling and body limits. Playwright runs the production build with a test-only API on `127.0.0.1:18765` and dashboard on `127.0.0.1:3007`. It exercises login/logout, token separation, unmanaged instance restrictions, workspace creation, each lifecycle operation, async job failures, empty/error states and mobile layout. It never calls a real fleet API or stops a developer workspace.

Tested initial dependency versions: Vinext 1.2.1, React 19.3.1, Vite 8.3.4. Exact versions and transitive integrity hashes are in `package-lock.json`. The scaffold was generated with the official `create-vinext-app --platform=node` workflow; see the [upstream setup and Node hosting docs](https://github.com/cloudflare/vinext#starting-a-new-vinext-project).

`npm audit` currently reports six high-severity dependency entries from the `braces` 3.0.3 glob-tooling chain. They trace to [GHSA-vfj7-8cjw-p6xm](https://github.com/advisories/GHSA-vfj7-8cjw-p6xm); 3.0.3 was still the latest published version at implementation time. The dashboard does not pass user input to these glob APIs. The `fflate` advisory was addressed with a compatible 0.7.5 override. Keep the remaining advisory visible during dependency upgrades; do not force-downgrade Vinext to satisfy the audit tool's suggested major-version replacement.
