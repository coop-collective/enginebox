# OAuth for connected apps

Engine Box's dashboard is an OAuth 2.1 authorization server that follows the
[MCP authorization specification](https://modelcontextprotocol.io/specification/draft/basic/authorization).
Any app can register itself, send the owner to a consent screen, and receive a
token that starts, reads, lists and cancels agent runs on the workspaces and
[pools](QUEUE.md#pools) the owner picked, and nothing else. No app gets special treatment: there is no client-specific
code or configuration.

Everything runs in the dashboard process on its public origin (for the hosted
install, `https://enginebox.coopcollective.dev` through the Cloudflare Tunnel and the
pass-through `dashboard-edge` Worker). The fleet API stays on loopback; the
dashboard calls it with the owner token only to mint and revoke service tokens and
to list workspaces, pools and profiles for the consent screen.

## Enabling it

Set `ENGINEBOX_OAUTH_STATE_DIR` to an absolute directory owned by the dashboard
user. Without it, every OAuth path answers 404. Under the supervisor, add it to the
private `environment_file` (it is on the allowlist). The directory is created with
mode 0700 and holds `oauth.json` (mode 0600) plus a short-lived lock file. Old and
candidate dashboards share it safely during an update: every change is a locked
read-modify-write with an atomic rename, and no network call happens under the lock.

## The flow

1. **Discovery.** A request to an agent-run route without a token gets `401` with
   `WWW-Authenticate: Bearer resource_metadata="<origin>/.well-known/oauth-protected-resource"`.
   That document names the authorization server (the same origin), whose metadata
   is at `/.well-known/oauth-authorization-server`.
2. **Registration** (RFC 7591). `POST /oauth/register` with `redirect_uris`,
   optional `client_name`, and `token_endpoint_auth_method` (`none` for public
   clients, `client_secret_basic` (default) or `client_secret_post` for confidential
   ones). A confidential client receives `client_secret` once.
3. **Authorization.** The app sends the browser to `/oauth/authorize` with
   `response_type=code`, `client_id`, `redirect_uri`, `code_challenge` +
   `code_challenge_method=S256`, `resource=<origin>`, optional `scope=agent-runs`
   and `state`.
4. **Sign-in and consent.** The owner signs in (if needed) and sees the app's
   self-chosen name, its redirect host and client ID, then ticks the pools and the
   workspaces (host/instance) it may use and, optionally, profile names per
   workspace. Pools come from the controller's owner-managed pool list; a pool's
   own profile is shown and is granted with the pool. Named profiles must exist on
   the controller. If one list cannot be read (for example, a host's inventory is
   unreachable), the other is still offered.
5. **Code.** Engine Box redirects to the exact registered URI with `code`, the
   original `state` and `iss`. Denial returns `error=access_denied`.
6. **Token.** `POST /oauth/token` with `grant_type=authorization_code`, `code`,
   `redirect_uri`, `code_verifier` (and client authentication). The response:

```json
{
  "access_token": "ebst_<id>.<secret>",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token": "ebrt_<grant>.<secret>",
  "scope": "agent-runs",
  "enginebox_grants": [
    { "kind": "workspace", "host": "primary", "instance": "ebox-work", "profiles": ["review"] },
    { "kind": "pool", "pool": "workers", "profile": "coder", "profiles": ["coder"] }
  ],
  "enginebox_callback_secret": "<43 characters>"
}
```

`enginebox_grants` lists one entry per consented workspace (the original
`{host, instance, profiles}` shape plus `kind`) and per pool. A pool entry's
`profile` is the pool's own profile (or null) and `profiles` repeats it as a list,
so clients that read `profiles` on every entry keep working.
`enginebox_callback_secret` is returned only by the code exchange: it is the HMAC
key the controller signs this connection's run [callbacks](FLEET.md#agent-runs-v2)
with, and it stays the same across refreshes. A refresh response repeats
`enginebox_grants` but not the key.

7. **Use.** Call the agent-run routes on the public origin with
   `Authorization: Bearer <access_token>`. Refresh with
   `grant_type=refresh_token` before `expires_in` runs out.

## Endpoints

| Path | Purpose |
|---|---|
| `GET /.well-known/oauth-protected-resource` | RFC 9728 metadata; `resource` is the origin |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 metadata |
| `POST /oauth/register` | RFC 7591 dynamic client registration |
| `GET /oauth/authorize` | Authorization request; renders sign-in or consent |
| `POST /oauth/authorize` | Sign-in and consent form posts (same-origin only) |
| `POST /oauth/token` | `authorization_code` and `refresh_token` grants |
| `POST /oauth/revoke` | RFC 7009 revocation of a refresh or access token |
| `GET /v1/agent-runs` | Run summaries for the token's workspaces |
| `POST /v1/hosts/{host}/instances/{instance}/agent-runs` | Start a run |
| `GET /v1/hosts/{host}/instances/{instance}/agent-runs/{id}` | Read a run |
| `POST /v1/hosts/{host}/instances/{instance}/agent-runs/{id}/cancel` | Cancel a run |
| `POST /v1/pools/{pool}/agent-runs` | Start a run on any free workspace of a granted pool |
| `GET /v1/pools/{pool}/agent-runs/{id}` | Read a pool run, placed or not |
| `POST /v1/pools/{pool}/agent-runs/{id}/cancel` | Cancel a pool run |
| `GET /api/oauth/grants`, `POST /api/oauth/grants/{id}/revoke` | Owner-only (dashboard session): list and disconnect apps |

The dashboard's **Connected apps** section lists every live connection (app name,
redirect hosts, workspaces, pools, profiles, last renewal) with a Disconnect button.

## Tokens and lifetimes

| Item | Lifetime and handling |
|---|---|
| Pending authorization request | 10 minutes; consumed by approve or deny |
| Authorization code | 60 seconds, single use. Any presentation burns it (a wrong verifier cannot be retried); a replay revokes the connection it produced |
| Access token | 1 hour. A scoped controller service token (`agent_runs.start/get/list/cancel` on exactly the consented workspaces and `pools`, with the named workspace profiles as its `profiles` grant and the connection's `callback_secret`), labelled `oauth grant <id>` |
| Callback key | The life of the connection; generated at the code exchange and passed to every access token minted for it |
| Refresh token | 30 days, sliding: each refresh issues a new one and resets the 30 days. Rotated tokens are remembered; presenting one again revokes the whole connection |
| Client secret | No expiry |

Codes, refresh tokens, client secrets and access tokens are stored only as SHA-256
digests. Each refresh mints a new service token and revokes the previous one on the
controller. Revoking a connection (owner Disconnect, a client revoking its refresh
token, refresh reuse or code replay) revokes the live access token too. If the
controller cannot be reached, the revocation is queued and retried on the next
OAuth call, and the public proxy already refuses the token: it accepts only the
*current* access token of a live connection, then the controller enforces scope.
Runs already admitted keep running until they finish or are cancelled.

If the controller refuses to mint a token for a connection (HTTP 400: a consented
workspace, pool or profile no longer exists), the code exchange or refresh answers
`invalid_grant` and a refresh also revokes the connection, so the app asks the
owner again instead of retrying forever.

## Security model

- **Sign-in** goes through a `UserAuthenticator` interface
  (`packages/dashboard/lib/oauth/identity.ts`). Today it is the operator password
  and the existing eight-hour session cookie, sharing the login rate limit; grants
  are keyed by user ID (`owner`), so per-tenant accounts can replace it.
- The session cookie is `SameSite=Strict`, so it is absent on the app's cross-site
  redirect. The first authorize response is a same-origin meta-refresh hop that
  reveals an existing session before asking for a password.
- Redirect URIs match exactly (including port and path). They must be `https`, or
  `http` on `localhost`, `127.0.0.1` or `[::1]`; no fragments, credentials or custom
  schemes. Errors about the client or redirect URI are shown on Engine Box and never
  redirected.
- Consent pages forbid framing, load no scripts, restrict `form-action` to Engine
  Box and the app's redirect origin, and accept posts only with a matching
  `Origin` (or `Sec-Fetch-Site: same-origin` when a browser withholds it).
- Only assigned, managed workspaces can be granted (not pool reserves or unmanaged
  bases), checked again against the live fleet on approval. Pools are checked
  against the controller's pool list the same way.
- Registration is open, as MCP expects: at most 30 per 10 minutes per process and
  200 clients in total (the oldest unused clients over a day old are dropped first).
- The `resource` parameter must be the origin (RFC 8707); it is required on
  authorization and checked on token requests when present.
- The agent-run proxy ignores cookies, forwards only the presented scoped token,
  allows only the seven agent-run route shapes (four workspace, three pool), and
  rejects query strings.
- Logs never contain tokens, codes or secrets; a failed revocation logs only a count.

## Not yet

- **Per-tenant user accounts.** One owner today. With accounts, each tenant's
  consent should offer only workspaces it owns, and scopes should name workspace
  UUIDs (see [tenant design](TENANT-AGENT-STATE.md)).
- **Profiles are granted per token, not per workspace.** The controller's
  service-token `profiles` grant applies to every workspace the token names, so a
  profile named on one workspace may be used on the others in the same connection.
- **The callback key is stored in `oauth.json`** (mode 0600), like the controller
  stores it with the service token, because every renewed token must carry it.
- **A credential vault** for the app's own secrets (for example repository tokens);
  apps pass those per run through `env` today.
- No browser CORS on the token endpoint (server and desktop clients only), no
  client-metadata-document clients, no scope narrowing on refresh, no consent memory
  (each authorization asks again), and no per-client registration management beyond
  disconnecting its grants.
