# Changes

## Unreleased

- **License.** New versions move from Sustainable Use License 1.0 to the Business Source License 1.1. Personal, non-commercial and non-production use is free; production business use requires a commercial license at $10 per node per month; hosted or managed service offerings require a separate agreement. Each version converts to Apache-2.0 four years after its first public release. Earlier versions keep their licenses; the Sustainable Use License text moved to `LICENSE-SUSTAINABLE-USE-1.0`. See [licensing](docs/LICENSING.md).

- **Project repositories.** Hosted Git repositories per project backed by Cloudflare Artifacts (beta): an owner-only, write-only backend configuration; create (idempotent), list, read and short-lived token routes; per-project `repositories` grants for service tokens and OAuth consent; and runs that name a repository receive a write token at admission as a git header scoped to that remote, revoked when the run ends. A backend interface leaves room for a GitHub App backend ([repositories](docs/REPOSITORIES.md)).

## 0.3.0 — development preview

Notable changes since 0.2.0, grouped from the commit history. Everything below remains a single-owner development preview; see the linked guides for what is qualified versus planned.

- **Rust runtime.** The runtime moved to native Rust packages with supervised, version-pinned release updates and retained-version rollback ([Rust migration](docs/RUST-MIGRATION.md), [updates](docs/UPDATES.md)). Native agent workspaces are preserved across the migration; Windows private-path and ACL handling and bounded command pipe draining were fixed.
- **LayerFS storage.** Opt-in LayerFS checkout backend managed through fleet tools, writable Incus checkouts exported through virtiofs, interrupted-command and daemon recovery, large Unreal project restore and merge qualification, and a choice of LayerFS container workspaces ([LayerFS](docs/LAYERFS-BACKEND.md), [game workspaces](docs/GAME-WORKSPACES.md)).
- **Cloudflare fleet.** Local Wrangler fleet with Durable Object authority, recoverable Store transport, R2 storage and candidate CI gates, with the design requiring Cloudflare fleet authority and optional storage nodes ([local fleet](docs/LOCAL-FLEET.md)).
- **Tasks and agents.** Durable project tasks with scoped native MCP tools, native Codex task-assistant dispatch, separation of model task credentials from trusted executor identities, and fencing of uncertain native runs before a restarted node advertises capacity ([task workflow](docs/TASK-WORKFLOW.md), [task assistant](docs/TASK-ASSISTANT.md)).
- **GitHub.** LayerFS-owned GitHub import and guarded main sync ([GitHub sync](docs/GITHUB-SYNC.md)).
- **ACP and CLI.** ACP control and a live native-agent fleet task CLI (`ebox tasks`, `ebox acp`) ([ACP](docs/ACP.md)).
- **Native clients.** Current Cursor CLI qualified with rejection of native version drift; explicit Cursor Cloud model probes recorded.
- **Site and layout.** Product site and developer guides; standalone research separated from product packages ([research](research/README.md)).
- **Measurements.** Rust VM boot benchmarks and remote agent timing campaigns; see [reports](docs/reports/VALIDATION.md).

## 0.2.0 — development preview

This release brings the portable-fleet development branch into the main repository and changes the license for new work to Sustainable Use License 1.0. Earlier Apache-2.0 grants remain valid; see [licensing](docs/LICENSING.md).

- Controller-managed native agent connections, automatically synchronized templates and private prepared workspace pools.
- Dedicated Fleet, Agents and Templates sidebar views.
- Operator MCP/API launch and polling for native Codex, Claude and Cursor runs, exact native session resume, private persistent workspace homes and bounded execution.
- Scoped Ubuntu sandbox preparation for Cursor's native helper, model-free readiness checks and updated native template recipes.
- Usage cards with read-only native Codex allowance/reset observations, stale-data handling and explicit unavailable billing data.
- Read-only portable host/resource/GPU discovery and explicit CPU/RAM contribution ceilings for the existing Linux backend.
- Test-gated Python wheel/source package builds, package notices and clean-environment installation checks.

This remains a single-owner development preview. Public multi-tenancy, movable tenant state volumes, Docker laptop enrollment, Windows/macOS native workers, GPU scheduling and Windows/macOS guest images remain unimplemented. Independent fresh-host installation, signed platform installers and upgrade/backup qualification remain release gates. Test fixtures do not establish hardware support or provider inference; exact observed validation is recorded in [VALIDATION.md](docs/reports/VALIDATION.md).
