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

The local regression qualifies the merge/export primitive. A separate combined test on the same owned Linux fixture then ran the real local Wrangler controller, Durable Object state, local R2 bindings and fleet `runNode` loop. Its controller and node sources came from `e110fc380a42a5ef76e11d87aad6000bc30e2288`; its Rust CLI used the combined integration/build source plus the corresponding GitHub source-only import fix. This was a local Cloudflare development stack, not a deployed Cloudflare service.

The combined flow published the base and two input snapshots, submitted both changes through the Enginebox API, and queued one frozen integration. The worker performed the actual same-Store merge and published the output generation. The controller moved both member tasks to Testing and automatically queued the two protected full build checks. Godot `4.7.2.stable.official.ed1daf0bf` produced Linux and Web artifacts from the exact exported tree; both checks completed successfully. The accepted head remained at the base while required playtest evidence and owner review were outstanding.

| Verified output | Identity |
| --- | --- |
| Published integration generation | `fd13b1dacbf3eba356d8990016854f35cad88f2dcb72b729dedf6234e1019976` |
| Frozen source tree | `cb517f34731b3420e21efe7b908b9a8a7c7d299f34ff2099c7b52e282b01bda3` |
| Protected Linux artifact | `f37df9d8aad505e4be857c06e542235db307d745f5c44cd98aa5e07ec04d33c5` |
| Protected Web artifact | `c6ce966fdb1117549af480cf5feb3a86a627dbcb192902beeedd9996a8fea32e` |

The test downloaded the Linux artifact through the controller, verified every file's length and SHA-256 against the immutable artifact manifest, and executed the downloaded entrypoint. It exited successfully with `ENGINEBOX_IMMUTABLE_PLAYABLE_OK`. This establishes execution of the published playable, in addition to successful export.

The application browser loaded the protected Web artifact through its authenticated Gamejam proxy, interacted with the real Godot button, and captured a playtest for each task. A separate enrolled CPU media worker finalized both recordings. Browser replay and FFmpeg decoding passed. The resulting WebM videos were 6,321 ms / 18,785 bytes and 3,533 ms / 38,331 bytes; both were 780 × 438. The policy required at least 1,000 ms of passing video. It explicitly did not require audio because this renderer supplied no game-audio stream; the test did not capture a microphone or claim audio qualification.

Owner review went through the application's Enginebox adapter. Approving only the first task left the batch in Testing. The second approval advanced the accepted head to the published generation above and marked both tasks Shipped at revision 4. An independent controller reread confirmed the accepted head, both task states, both Ready recordings, and the original completed protected build jobs. Acceptance reused those immutable artifacts; it did not rebuild the candidate. Private receipts, videos and screenshots were retained outside Git before fixture cleanup.

Cross-Store import, distributed writable SQLite, arbitrary binary conflict resolution, power-loss durability and Unreal/Unity cold startup are not established by these results. The 128 MiB source asset remains synthetic; it does not substitute for qualification with a genuine large Unreal project. Fixture cleanup must verify its exact ownership, loop backing file and container identities; it must never prune unrelated resources.
