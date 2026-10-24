# Personal machine registration and WebRTC signaling

This controller owns registration, process fencing, viewer grants, control leases,
and a bounded SDP/ICE mailbox. Video and input travel over WebRTC between the
browser and the isolated native runtime. The controller has no host URL, inbound
machine port, frame upload API, provider execution, or media-over-WebSocket path.

The `native` role is restricted to one project. It cannot access Store objects,
agent jobs, artifacts, test evidence, task APIs, or provider credentials. Existing
agent/storage registrations and DO classes are unchanged. These records use the
existing FleetNode, Registry, and Project SQLite-backed Durable Objects; no DO
migration or merge of SQLite files is involved.

## Identity and ownership

A trusted Account service authenticates the human and checks project membership.
It keeps the fleet owner token outside the room, browser and native container.
Only that service creates invitations and viewer grants. A room's task-only
credential can list project machines and relay a granted session; it cannot mint
a human identity, select control policy, register a host, or revoke a machine.

All identifiers use the existing lowercase/digit/hyphen identifier format, up to
48 characters. Labels are bounded to 120 UTF-8 bytes; account pseudonyms to 128.
All timestamps are epoch milliseconds. All authenticated responses are no-store.

Owner endpoints below are relative to `/v1/projects/:project`:

| Method and path | Request | Response |
| --- | --- | --- |
| POST `/machine-invitations` | `{id,label,ownerAccountId,allowControl}` | `{node,token,expires}` |
| GET `/machines` | Owner or project task-only credential | `{machines:[{id,label,ownerAccountId,allowControl,online,epoch,lastSeen,stats,revoked,pending,invitationExpires?}]}` |
| POST `/machines/:id/revoke` | `{operationId}` | `{revoked:true}` |
| POST `/machine-session-grants` | `{id,sessionId,nodeId,actorId,mode:"watch"|"control"}` | `{grant,expires,actorId,sessionId,nodeId,mode}` |

Invitations last 10 minutes. Exact pending retries return the same token and
expiry. Changed settings, expired IDs, revoked IDs, and consumed invitation
creation requests return 409; use a new ID for a replacement host. A host token
is generated and persisted locally **before** claiming the invitation. Redeeming
`<node>.enroll.<64hex>` at `POST /v1/node/enroll` with
`{nodeSecretHash:SHA256("<node>.<64hex>")}` atomically claims the identity. The
same persisted hash can retry until original invitation expiry; another hash
cannot take over. The invitation is never an owner, app, or normal node token.

The host uses a fresh UUID per process for `POST /v1/node/hello {session}`. The
response is `{epoch,session,reservations:[]}`. Native process UUIDs are remembered;
a fenced UUID cannot reclaim the epoch. At 4096 boots, enroll a new identity.
Following host requests carry its bearer plus `x-enginebox-epoch` and
`x-enginebox-session`. Heartbeats every 5 seconds publish bounded numeric stats;
grants require a heartbeat within 20 seconds and `webrtc:1,video:1` (`input:1` for
control). A new process clears previous readiness stats. Native 401/403/409 means
stop all peers and exit; do not silently steal another epoch.

The controller requires `allowControl` for a control grant and permits one
exclusive control session per host. The trusted Account service separately
checks the real user's permission (for example owner/builder versus viewer).
An app credential or arbitrary actor string is not sufficient authority.

## Session and signal contract

The browser first saves 32 random bytes as a 64-hex client secret and submits its
SHA256 with the fresh Account grant. All subsequent client calls require the
project task-only bearer and `x-enginebox-stream-client:<secret>` (never a query
parameter). The room verifies its current authenticated principal equals the
immutable `session.actorId` before forwarding any session read/signal/renew/end;
this also prevents account-switch reuse of an old browser secret.

| Method and path under the project | Request | Response |
| --- | --- | --- |
| POST `/machine-sessions` | `{id,grant,clientSecretHash}` | `{session,iceServers,iceTransportPolicy,iceExpires}` |
| GET `/machine-sessions/:id` | Client secret, owner, or matching current native host | `{session}` |
| GET `/machine-sessions/:id/signals?after=0` | Client secret or matching current native host | `{session,signals:[{sequence,id,signal}],cursor}` |
| POST `/machine-sessions/:id/signals` | `{id,signal}` | `{sequence,session}` |
| POST `/machine-sessions/:id/renew` | `{grant}` plus client secret | `{session,iceServers,iceTransportPolicy,iceExpires}` |
| POST `/machine-sessions/:id/end` | `{id}` | `{session}` |

A session includes `id,project,nodeId,nodeEpoch,actorId,mode,state,expires,createdAt`
and after closure `endedAt,reason`. Reasons are `client,host,owner,expired,fenced,
revoked`. A lease lasts 60 seconds; obtain a new Account grant and renew around 30
seconds. Retrying the same grant cannot extend its original lease window.
Expired sessions cannot resume. Active reads/signals/renew on ended sessions
return 410 so a native host can close that peer without terminating unrelated
peers. Reopening an ended ID returns 409.

`POST /v1/node/machine-sessions/poll {}` returns `{sessions,now}`. Every own
current-epoch live session includes its exact `iceServers,iceTransportPolicy,
iceExpires` fields. Polling every 2 seconds performs no TURN API calls. Missing
sessions must close locally. Host and viewer independently enforce lease expiry
with a monotonic deadline; controller loss must never extend authority.

A signal is exactly one of:

- `{type:"offer",sdp}` from the client, once, at most 64 KiB.
- `{type:"answer",sdp}` from the native host, once after the offer, at most 64 KiB.
- `{type:"ice",candidate:{candidate,sdpMid,sdpMLineIndex}|null}` from either side;
  the candidate is at most 4096 UTF-8 bytes, at most 128 ICE messages per side.

Only the opposite side's signals are returned; `cursor` advances over the whole
session sequence. Stable operation IDs with identical content are idempotent;
changed content returns 409. At most 258 messages exist per session. There is no
renegotiation in a session; open a new session when required. Owner credentials
can end a session but cannot inject or read signaling through these routes.

## Relay configuration and bounds

Set the protected optional `MACHINE_TURN_CONFIG` secret to
`{"keyId":"...","apiToken":"...","ttlSeconds":3600,"iceTransportPolicy":"relay"}`.
The server calls Cloudflare's TURN credential API only after grant authorization.
Only returned short-lived ICE credentials reach the client/native host; the
provider API token never leaves the controller. Missing configuration returns 503
for a new grant, while enrollment/status remain available. Renewals reuse the
session's original ICE credentials and make no upstream call. Session lifetime
is capped to the earlier of one hour and initial ICE expiry minus 30 seconds;
clients must reconnect rather than assuming a new grant refreshes an existing
TURN allocation. Configured TTL may be 120–3600 seconds.

An explicitly local fixture may use
`{"local":true,"iceServers":[...],"iceTransportPolicy":"all"|"relay"}` only when
`ALLOW_HTTP_NODES=true`. This is test configuration, not production relay proof.

Limits are 8 simultaneous peers per host, one control peer, 64 live peers per
project, 4096 retained sessions/project, and 4096 recent grants/project. Grant
secrets/ICE envelopes are collected 5 minutes after expiry; grant operation IDs
must be unique. Ended sessions and their mailbox/idempotency records are retained
for 24 hours then collected on subsequent activity. These bounds can return 409 or
429 under capacity pressure. Native boot history and revoked identities persist;
no claim suggests unlimited registration or distributed media scheduling.

## Validation

`pnpm --filter @enginebox/fleet-controller test` runs actual workerd/SQLite tests.
`test/machines.test.ts` covers concurrent invitation claims and control admission,
retry/eviction, cross-project and role denials, private signaling, mailbox bounds,
process fencing, revocation, grant collection, relay reuse and expiry. Broker
tests cover protected upstream requests and failure behavior. These tests prove
controller authority, not video capture, NAT traversal, GPU support, or production
Account login; the stream-host/browser qualification covers the media path.
