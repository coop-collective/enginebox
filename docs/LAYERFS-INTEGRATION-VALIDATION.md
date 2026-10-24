# Same-Store logical integration qualification

On 2026-10-24, the product's pinned LayerFS revision `968a5d0be0ea3e9456b06f9148b65523138e9490` completed a disposable Linux integration test with a Godot source fixture and a **128 MiB incompressible synthetic binary asset**. This is not a genuine Unreal project or an Unreal editor cold-start measurement.

The executable regression is `packages/enginebox-storage-layerfs/tests/live-integration.mjs`. It requires explicit opt-in, an owned fixture marker, a new configured Store root and a fresh report. It invokes the real Rust CLI, pinned native SDK, FUSE sandboxes and online SQLite backup. No model inference is involved.

The verified source contained 134,219,644 bytes. Its initial SQLite backup occupied approximately 129.6 MiB, so the large-file test did not rely on repeated-byte deduplication. Checks passed for:

- A two-change logical merge into a fresh private branch, with both edits present.
- The full binary file length and SHA-256, every exported file hash, and the canonical source-tree hash.
- SQLite integrity and foreign keys in the exported backup.
- Identical request replay returning the recorded result without another workspace.
- Overlapping text, binary and `.lww` conflict-policy refusals, including idempotent refusal replay.
- The original LayerFS main reference and filesystem root remaining unchanged.

The source-tree hash for the incompressible fixture was `cb517f34731b3420e21efe7b908b9a8a7c7d299f34ff2099c7b52e282b01bda3`. The first, separately retained export test used a repeated-byte asset; it is not the incompressible measurement above.

The environment was Linux kernel `7.0.0-30-generic`, Node `24.21.0`, Rust `1.96.0`, ext4 and an isolated Docker `29.5.3` daemon with API `1.54`. The official DinD image was pinned to `docker@sha256:7278248384185bcbb54c7a76b2f0a7d354c4aac920605cc1ab48968d961052fb`. The fixture had a fixed 16 GiB ext4 image, a 4 CPU / 8 GiB outer limit, and no host Docker socket or provider credentials. The host daemon was not upgraded. A prior minimal VM request was refused by native fleet admission; no VM was created and the disk budget was not raised.

Two implementation issues were found by the actual runtime test and corrected before the successful result: direct Docker copying silently omitted the dynamic FUSE mount, and legacy Git-bearing imports contained metadata that the source exporter correctly refused. The executor now archives from inside the immutable mount namespace; logical integration uses source-only projects. Existing unqualified project receipts are not silently adopted.

This regression qualifies the local merge/export primitive. The controller queue, R2 generation publication, protected engine build, playable artifact and owner review are a separate combined proof. Cross-Store import, distributed writable SQLite, arbitrary binary conflict resolution, power-loss durability and Unreal/Unity cold startup are not established by these results. Fixture cleanup must verify its exact ownership, loop backing file and container identities; it must never prune unrelated resources.
