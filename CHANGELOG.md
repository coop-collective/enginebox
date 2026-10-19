# Changes

## 0.2.0 — development preview

This release brings the portable-fleet development branch into the main repository and changes the license for new work to Sustainable Use License 1.0. Earlier Apache-2.0 grants remain valid; see [licensing](docs/LICENSING.md).

- Controller-managed native agent connections, automatically synchronized templates and private prepared workspace pools.
- Dedicated Fleet, Agents and Templates sidebar views.
- Operator MCP/API launch and polling for native Codex, Claude and Cursor runs, exact native session resume, private persistent workspace homes and bounded execution.
- Scoped Ubuntu sandbox preparation for Cursor's native helper, model-free readiness checks and updated native template recipes.
- Usage cards with read-only native Codex allowance/reset observations, stale-data handling and explicit unavailable billing data.
- Read-only portable host/resource/GPU discovery and explicit CPU/RAM contribution ceilings for the existing Linux backend.
- Test-gated Python wheel/source package builds, package notices and clean-environment installation checks.

This remains a single-owner development preview. Public multi-tenancy, movable tenant state volumes, Docker laptop enrollment, Windows/macOS native workers, GPU scheduling and Windows/macOS guest images remain unimplemented. Independent fresh-host installation, signed platform installers and upgrade/backup qualification remain release gates. Test fixtures do not establish hardware support or provider inference; exact observed validation is recorded in [VALIDATION.md](docs/VALIDATION.md).
