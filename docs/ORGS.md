# Organizations, members and email sign-in

Engine Box can serve a team: each person signs in with their email, works in one or more organizations, and the install's own machines stay under the operator's control. This page describes the model, sign-in, tenancy and where a managed (hosted) offering plugs in. Setup steps are in [Self-hosting orgs](SELF-HOSTING-ORGS.md).

Orgs are **off** unless `ENGINEBOX_IDENTITY_STATE_DIR` is set. Off, the dashboard behaves exactly as before: one operator password, every record in the platform org.

## Model

| Concept | What it is |
| --- | --- |
| **Instance owner** | The operator of the install. The break-glass operator session, plus the user named by `ENGINEBOX_OWNER_EMAIL` (or claimed by the operator in **Organization**). Owns the platform org. |
| **Platform org** (`default`) | Shown as **Default Org** on a self-hosted install (`ENGINEBOX_ORG_NAME` overrides). Owns the install's nodes, pools, profiles and repositories. Everything created before orgs belongs to it. |
| **User** | One per email address. Every user gets a **personal org** (their personal workspace) when the account is created. |
| **Team org** | Created by any signed-in user (**Organization → New team organization**). People join by invite. |
| **Member** | A user's membership in one org, with role `owner`, `admin` or `member`. |
| **Mode** | `ENGINEBOX_TENANCY_MODE`: `self_hosted` (default) or `managed`. |

Roles, per org:

- **member**: read the org's fleet, start agent runs on its pools, allocate and operate its workspaces, create hosted repositories, run day-to-day storage reads.
- **admin**: also invite and remove members and admins, connect the org's controller-scope agent accounts, approve OAuth apps, manage templates, GitHub, CI and storage maintenance (the last three only in the platform org), enroll machines into the org.
- **owner**: also manage owners and rename the org. An org always keeps at least one owner.

A personal org has only its owner: no invites, no removal. In `managed` mode only the instance owner adds people to the platform org; tenants never see or join it.

### Self-hosted and managed from one codebase

- **self_hosted**: one install, the Default Org, no billing. The instance owner can use everything the platform org owns, invite members into the Default Org, or share a Default Org pool with another org through a capacity grant. The Default Org never sees a personal or team org's resources.
- **managed**: the platform org is the provider's. Its nodes reach tenant orgs only as **shared capacity** through capacity grants (or quotas implemented on top of them). Tenants have no instance-owner powers. Open sign-up is on: an unknown address that verifies a code gets an account and its personal org. (Self-hosted installs can enable it with `ENGINEBOX_OPEN_SIGNUP=1`.)

### Node ownership

Every host and enrolled node belongs to exactly one org. The controller's `tenancy.json` maps host or node ID to org; anything unlisted is the platform org's, so no migration is needed. An org brings its own machines by enrolling them from its **Compute** view (Windows/WSL workers through the existing contributor enrollment); the dashboard records the enrolling org with `POST /v1/tenancy/hosts/{id}/org`. Configured Linux servers are part of the install's own configuration and belong to the platform org until the instance owner assigns them. An org sees and operates only its own machines.

### Capacity grants

A grant is: platform org → tenant org, one pool (or an opaque capacity class), and limits (`max_concurrent_runs`, `cpu`, `memory_gib`, `monthly_hours`).

```http
POST /v1/tenancy/grants        {"tenant_org":"org_…","pool":"shared","limits":{"max_concurrent_runs":2}}
POST /v1/tenancy/grants/{id}/revoke   {}
GET  /v1/tenancy               → {mode, platform_org, host_orgs, grants}
```

Owner token only. The controller admits a tenant's pool run only for its own pools or a platform pool under a live grant naming it; the platform org never reaches a tenant pool. **Implemented in this release:** the grant records, the admission check and the dashboard showing granted pools to the tenant. **Not yet enforced:** the limits themselves and capacity-class grants. The admission result carries the grant's limits for the quota layer (see Extension points).

## Records and org scoping

| Record | Where the org lives |
| --- | --- |
| Pools, profiles (controller) | `org_id` on the record; absent means the platform org and is never written for it, so releases before orgs still read every platform record. |
| Connections (controller) | `org_id` on every record (`connections/`); names are unique per org and a profile resolves connections only in its own org. See [CONNECTIONS.md](CONNECTIONS.md). |
| Audit log (controller) | `org_id` on every entry (`audit/events.jsonl`); `GET /v1/audit` returns the acting org's. |
| Hosted repositories | `org_id` on the record. Creating a repository whose name another org already uses in that project is refused instead of returning the other org's record. |
| Service tokens / OAuth access tokens | `org_id` on the token, from the OAuth grant's org. |
| OAuth grants (dashboard) | `org_id` on the grant (absent: platform org). The approver keeps managing it; so do that org's admins. |
| Hosts and nodes | `tenancy.json`. |
| Controller-scope agent accounts | The platform org's team accounts. |
| Org agent credentials | `org_id` on each record (`agent-credentials/`), see below. |

The dashboard holds the owner token. For orgs other than the platform org it sends `X-EngineBox-Org: <org>` with it; the controller stamps new pools, profiles and repositories with that org and refuses runs on another org's hosts or pools. Platform-org requests carry no header and are byte-for-byte what they were before orgs. The public `/v1` owner-token API therefore keeps acting as the platform org's owner, and existing OAuth integrations such as Ideator keep working unchanged.

## Agent credentials

Each org keeps its own agent accounts (`GET/POST /v1/credentials`, acting org from `X-EngineBox-Org`; dashboard **Agents → Your and your team's agent accounts**, with a **Unity account** card for your own Unity license):

| Kind | Who adds it | Connect | Sharing |
| --- | --- | --- | --- |
| `personal` | any member, under their own identity | the provider's native sign-in (the same flow as controller accounts) into a private home of its own; only its owner can sign it in | the owner alone decides **Share into org pools** |
| `team` | org admins | native sign-in by an admin | org-owned |
| `api_key` | org admins | the key is sent once, stored in a 0600 file and never returned or logged | org-owned |
| `personal`, provider `unity` | each member, for themselves only | a Unity ID email and password, checked with Unity when saved (see [Unity accounts](#unity-accounts)) | never shared: used only by sessions its owner starts |

Members see team accounts, keys (metadata only), their own personal accounts and others' shared ones; admins see all metadata and can remove any credential. The controller-scope accounts (`GET /v1/agents`) remain the platform org's team accounts.

Runs do not select from these yet. `credentials::Credentials::candidates(org, provider, member)` is the selection interface the credential-pool work will call: connected credentials of the org for the provider, in order: the requesting member's own personal accounts, then personal accounts their owners shared, then team accounts, then API keys. An org pool run without a member skips unshared personal accounts. Usage events already carry `credential_id`.

### Unity accounts

A member's Unity license comes from their own Unity ID: **Agents → Unity account** takes an email and password. Rules:

- **One per member per org.** `POST /v1/credentials {provider: "unity", kind: "personal", username, password}` saves it (the dashboard fills `owner_user_id` from the session; the label is the username). A second save answers **409**: change the saved one with `POST /v1/credentials/{id}/update {username?, password}` instead. Remove it with `POST /v1/credentials/{id}/delete`.
- **Owner only.** Only the member saves or updates their own; admins may remove any (as for other personal credentials). There are no team, shared or API key Unity accounts, and sign-in and sharing do not apply. The dashboard names the member in `X-EngineBox-Actor`, and the controller refuses a save or update for anyone else (403).
- **Checked when saved.** The controller signs in once (`POST https://core.cloud.unity3d.com/api/login`, `grant_type: password`; 10 s timeout, no redirects; `ENGINEBOX_UNITY_LOGIN_URL` overrides the endpoint):

  | Unity answers | Result |
  | --- | --- |
  | an access token | saved, `status: connected`, `validated_at` set |
  | 401 / `143.002` "Invalid Credential" | refused (400), nothing stored; an update leaves the saved account unchanged |
  | a two-factor or verification step | refused (400): turn off two-factor sign-in for that Unity account, because a password sign-in cannot answer it |
  | no answer, 5xx, 429 or an unrecognized reply | saved, `status: unverified`, `last_error` `unreachable`, `service_error` or `unexpected_response` |

  So a saved account always worked when it was saved, unless Unity could not be asked.
- **Write-only.** The password is stored in a 0600 file and never returned, logged or audited. Listings show `username`, `status`, `validated_at` and `last_error`. Saves, updates and removals are audited as `credential.unity_save`, `credential.unity_update` and `credential.unity_delete` with the outcome and a value-free reason.
- **Your sessions use yours.** `Credentials::unity_for(org, member)` returns only that member's own account, with no fallback to anyone else's. `Credentials::unity_access_token(org, member)` signs it in and returns a short-lived access token for a node to run `Unity.Licensing.Client --activate-all --accessToken <token> --include-personal`; the password never leaves the controller. Engine sessions will call it; nothing does yet.

Unity licensing is not a [connection](CONNECTIONS.md): connections are org-wide and shared by profiles, while a Unity license belongs to one person.

## Connections

Each org has its own [connections catalog](CONNECTIONS.md): MCP servers and secret sources that the org's profiles share by name. Only org **owners and admins** create, change, test, delete or attach connections and apply catalog files; the dashboard enforces the role and forwards the member's user ID as `X-EngineBox-Actor`, which the controller records as `created_by`/`updated_by` and in the audit log. Members see which connections a profile uses, never a secret value (nobody does: values are write-only). Kitbash deliveries belong to the platform org unless the delivery names another org ([CONNECTIONS.md](CONNECTIONS.md#kitbash)). Connection tests from orgs other than the platform org only reach public addresses.

## Sign-in

### Email codes and magic links

1. `POST /api/auth/email {email}` always answers `{sent: true, challenge, expires_minutes}` for a well-formed address. For an address without an account the challenge is random and never verifies, the same limits apply and no email is sent; delivery happens after the response is decided, so neither the body nor the timing says whether the account exists.
2. The email carries a 6-digit code and a link. Both expire in 10 minutes and work once; requesting a new code ends the previous one; five wrong codes end a challenge.
3. `POST /api/auth/email/verify {challenge, code}` signs in. The link (`/api/auth/link?c=&t=`) shows a confirmation button and signs in only on its POST, because mail scanners prefetch links.

Codes, link tokens, invite tokens and session IDs are stored only as HMAC-SHA256 digests keyed by `ENGINEBOX_SESSION_SECRET`, domain-separated per purpose.

Rate limits (sliding windows, persisted in the identity state so restarts and the supervisor's old/new dashboards share them; keys are digests):

| Limit | Value |
| --- | --- |
| Start, per email | 5 / 15 min |
| Start, per client address | 20 / 15 min |
| Start, global | 200 / 15 min |
| Verify, per client address | 30 / 15 min |
| Verify, global | 500 / 15 min |
| Invite accept, per client address | 20 / 15 min |

The client address comes only from the header named by `ENGINEBOX_CLIENT_IP_HEADER` (set `cf-connecting-ip` behind the Cloudflare tunnel, where the edge sets it and the origin is not reachable otherwise). Unset, all clients share one bucket, still bounded by the per-email and global limits.

### Invites

Admins invite by email (`POST /api/org/invites {email, role}`). The invite is single use, expires in 7 days, can be revoked and replaces an earlier pending invite for the same address. Opening the link shows the invite; accepting (a POST) creates the account with its personal org if needed, adds the membership and signs the browser in. The link reached that address, so accepting proves control of it. Owners alone invite owners.

`POST /api/org/members/import {members: [{email, role?, name?}]}` adds people directly, matched by email, without sending mail: the way to carry a team over from another system such as Ideator. They prove the address the first time they sign in with a code.

### Sessions

The existing `enginebox_session` cookie (HttpOnly, SameSite=Strict, Secure on HTTPS, eight hours) carries one of:

- an **operator** session: signed with the session secret and the operator password, stateless, as before (cookies issued before orgs keep working until they expire);
- a **member** session: `{kind: "member", sid, expires}`, signed with the session secret alone, plus a server-side record (`sid` digest, user, active org, expiry, revocation). It is valid only while the record is live and the user still belongs to the org. Sign-out revokes the record; removing a member revokes their sessions in that org; `POST /api/org/switch {org_id}` moves the session to another of the user's orgs.

### Break-glass operator password

`ENGINEBOX_DASHBOARD_PASSWORD` stays as the host operator's emergency login, labelled as such on the sign-in page and on the OAuth consent page. It acts as the platform org's owner and the instance owner. It is how the first owner is named when `ENGINEBOX_OWNER_EMAIL` is not set: **Organization → Name the instance owner** sends a code to that address, and verifying it in the operator session creates the owner and switches the browser to that member session.

## Email sending

Sending lives in a small Worker, `enginebox-email-gateway` (`packages/email-gateway`), with a Cloudflare Email Sending binding restricted to one sender address. The dashboard signs a fixed-shape message with `ENGINEBOX_EMAIL_SECRET` and posts it to `/_email/send` on the site Worker, which forwards it over a service binding (RPC). The gateway verifies the signature (±300 s, single-use nonce in a Durable Object), renders one of three fixed templates and sends. This mirrors the Artifacts gateway.

Why there:

- The controller host holds no Cloudflare API token; the binding needs none, and the gateway has no hostname, workers.dev or preview URL.
- The gateway renders the message itself. Codes must be 6 digits and links must point at `DASHBOARD_ORIGIN`'s sign-in or invite paths, so a leaked dashboard secret can only send Engine Box sign-in mail for this dashboard, never arbitrary content to arbitrary people.
- The site Worker stays secret-free; without the binding `/_email/send` answers 503.

Development and tests use `ENGINEBOX_EMAIL_TRANSPORT=file`, which appends messages to `<identity dir>/outbox/outbox.jsonl` (0600).

## Configuration

| Variable | Purpose |
| --- | --- |
| `ENGINEBOX_IDENTITY_STATE_DIR` | Absolute private directory for `identity.json`. Enables orgs. Production: `<controller state>/identity`. |
| `ENGINEBOX_OWNER_EMAIL` | The instance owner's email (bootstrap). |
| `ENGINEBOX_ORG_NAME` | Platform org display name (default "Default Org", "Platform" in managed mode). |
| `ENGINEBOX_EMAIL_ENDPOINT` | `https://<site>/_email/send`. |
| `ENGINEBOX_EMAIL_SECRET` | Shared HMAC secret (≥ 32 characters), the gateway's `EMAIL_GATEWAY_SECRET`. |
| `ENGINEBOX_CLIENT_IP_HEADER` | Trusted client-address header, for example `cf-connecting-ip`. |
| `ENGINEBOX_TENANCY_MODE` | `self_hosted` (default) or `managed`. Read by the dashboard and the controller. |
| `ENGINEBOX_OPEN_SIGNUP` | `1` allows self-service accounts on a self-hosted install. |

All of these are accepted in the supervisor's `environment.json`.

## Extension points (managed offering)

The core (controller, guest, node agent and dashboard with orgs) is complete and usable self-hosted; nothing in it depends on managed-only code. A managed offering adds hosted control-plane operations, shared capacity, metering and billing outside the core, through these seams:

- **Metering**: `enginebox_controller::metering::UsageSink`. The controller emits a `UsageEvent` (org, capacity org, grant, run, credential, pool or host, who started it) when it admits a run. The default sink discards events. A deployment installs its own with `Controller::with_usage_sink`. Pricing, aggregation and invoices are not in the core.
- **Quotas**: `tenancy::Tenancy::admit_pool_run` returns `Admission::Granted { grant_id, limits }`; a quota layer enforces the limits (and resolves capacity-class grants) before or around admission.
- **Mode**: `ENGINEBOX_TENANCY_MODE=managed` turns off instance-owner powers for tenants, hides the platform org from them and enables open sign-up.
- **Credential selection** (next): runs will pick an agent credential through `Credentials::candidates`; usage events already carry `credential_id`.

## Migration and rollback

- Enabling orgs creates `identity.json` with the platform org and, when `ENGINEBOX_OWNER_EMAIL` is set, the instance owner and their personal org. No controller record is rewritten: platform-org records never gain an `org_id` field, `tenancy.json` is created only on the first assignment or grant, and every existing OAuth grant and service token remains the platform org's.
- Rolling back the release is safe while only platform-org records exist. Records stamped with another org's `org_id` (pools, profiles, repositories, service tokens) are refused by releases before orgs, which reject unknown fields; remove or reassign them before rolling back.
- Turning orgs off again (unsetting `ENGINEBOX_IDENTITY_STATE_DIR`) ends member sessions; the operator password keeps working.
