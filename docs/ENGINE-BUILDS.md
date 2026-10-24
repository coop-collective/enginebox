# Exact-source engine builds

Enginebox owns reusable engine execution. A Gamejam task supplies requirements through the task API; the trusted integration worker freezes and merges its LayerFS source, then runs an owner-configured recipe. The build emits a content-addressed playable artifact. Candidate validation, review and acceptance refer to that same artifact; acceptance does not start another build.

`enginebox-build` is a native Rust executable using Enginebox's existing bounded process supervisor and inherited workspace lock. It is not an agent planner, cloud service or an engine installer. Build project code runs inside the native worker's isolated workspace boundary. A same-UID directory is not a sandbox for hostile projects. Keep controller, provider and fleet owner credentials outside that boundary.

## Commands

```sh
cargo build --locked --release -p enginebox-engine-builds
# All paths below are absolute, trusted worker paths.
target/release/enginebox-build fingerprint --recipe /private/recipes/godot-web.json
target/release/enginebox-build build \
  --recipe /private/recipes/godot-web.json \
  --source /private/candidate/source.json \
  --root /private/candidate/frozen \
  --state /private/builds \
  --lease /private/attempt/lease.json
```

The only successful stdout record is `{artifactId,manifestPath,filesRoot,cacheHit}`. Diagnostics and failures use stderr. `manifestPath` and `filesRoot` are private local paths for the trusted publisher, never client-controlled upload paths. Logs remain bounded files under the private state directory and are not CI evidence. JSON inputs reject unknown fields.

Native macOS and Linux are supported for the qualified Godot path. Fleet workers use Linux containers/VMs; Windows/macOS local controllers can call that Linux worker. Native Windows engine execution, Windows exports, and macOS bundle exports are not qualified by this adapter. Output symlinks are deliberately refused; macOS framework bundles need a separate packaging recipe before support is claimed.

## Frozen source

The source schema is `{schema:1,project,generation,layerfs:{revision,project,ref,root},treeHash,entries}`. `revision` is the pinned LayerFS commit hex string, not a counter. Generation and treeHash are lowercase SHA256. Each entry is either `{kind:"file",path,sha256,size,executable}` or `{kind:"symlink",path,target}`. Entries are sorted by relative UTF-8 path. `treeHash` hashes the entries array as canonical JSON: recursively sorted object keys, compact serialization, UTF-8, preserved array order.

Before any engine starts the worker checks the complete inventory, file bytes/modes, treeHash and generation syntax. Symlinks must be relative and resolve inside the frozen tree. Absolute paths, traversal, special files, case-colliding files and extra source files fail closed. A native builder cannot prove that an arbitrary local folder belongs to a transport generation: the trusted LayerFS exporter and fleet dispatcher bind the transport SHA to this manifest. Do not accept a manifest supplied directly by an agent/browser as evidence.

Builds reconcile a separate private copy, preserving unchanged file mtimes. The frozen source is never mutated. Enginebox replaces Godot export presets only in this copy. Source exports must exclude engine-generated caches and credentials.

## Protected recipes

A Godot Web recipe looks like this (replace the example hashes with actual SHA256):

```json
{
  "schema": 1,
  "engine": "godot",
  "version": "4.7.2.stable.official.ed1daf0bf",
  "target": "web",
  "architecture": "wasm32",
  "executable": "/opt/godot/Godot",
  "executableSha256": "<64 lowercase hex>",
  "inputs": [{"name":"export-template","path":"/opt/godot/web_nothreads_release.zip","sha256":"<64 lowercase hex>"}],
  "entrypoint": "index.html",
  "timeoutSeconds": 300
}
```

Linux uses target `linux`, architecture `x86_64` or `arm64`, its matching `linux_release.*` template, and a relative executable entrypoint. Godot's reported version must exactly match the recipe. The Web adapter uses the no-threads template and compatibility rendering must be selected by the project. Linux embeds the PCK. Import and release export share one overall deadline, including source/tool verification.

Optional fields are `maxLogBytes` (default 8 MiB, maximum 16 MiB), `cacheVersion` (default `"1"`), `environment` (an explicit small SDK configuration allowlist), `unityMethod`, and `unrealProject`. Inherited environment is cleared. HOME, XDG and temporary directories are private cache-namespace directories; engine/provider/operator tokens are not inherited. License files can be explicitly delegated through `UNITY_LICENSE_FILE`; the owner must limit that delegation to the intended trusted project worker.

`fingerprint` returns the recipe SHA and `{engine,version,target,architecture,fingerprint}` toolchain identity. Recipe SHA hashes canonical `{adapter:1,recipe:<normalized recipe with all defaults>}`. It includes configured paths by hash, so moving an installation conservatively invalidates recipe cache. Toolchain fingerprint hashes adapter version, engine/version/target/architecture, executable bytes and sorted input names/hashes. All tool bytes are checked before cache reuse and again before committing new output. A changed toolchain cannot silently inherit the previous cache identity.

Unity and Unreal command recipes are implemented but **not engine-qualified** by the Godot tests:

- Unity targets WebGL or Linux x86_64. Set `unityMethod` to a project static build method that reads the `-engineboxOutput` argument, runs `BuildPipeline.BuildPlayer`, and exits nonzero on a failed report. Enginebox passes batchmode, nographics, quit, projectPath, buildTarget and stdout log flags. A licensed editor and platform modules must already exist.
- Unreal targets Linux x86_64. `executable` is the installed executable RunUAT launcher, `unrealProject` is a relative `.uproject`, and `entrypoint` identifies the archived executable. The recipe invokes unattended BuildCookRun with build/cook/stage/pak/archive. Engine, compiler, SDKs and plugins must already exist. Shader/asset compatibility is enforced by the toolchain/cache namespace and the engine itself, not an assumed universal cache format.

Both require an input named `toolchain-lock`. Its JSON is `{schema:1,root:"/opt/engine",entries:[...]}` using the same complete entry format as a source manifest. The executable must be inside this locked installation. Enginebox streams and verifies every listed file and rejects unlisted files; hashing only the launcher or a descriptive version file is insufficient. Additional SDK installations use `toolchain-lock-sdk` or similarly named inputs. Keep these lock files outside the immutable installation they inventory. Windows reparse points and symlinked external SDK layouts require separate qualification; do not weaken the lock to accommodate them.

## Cache, cancellation and publication

Cache keys bind project, generation, source tree, recipe and toolchain. Cached output is fully rehashed, including exact inventory and executable mode, before returning its artifact ID. Corruption is an error; it does not silently rebuild over the damaged artifact. An operator can remove an inactive corrupt entry and retry. Artifacts use `state/artifacts/<buildKey>/manifest.json` and `files/`; the build key is distinct from the manifest's content-addressed artifact ID.

Generated caches are local, rebuildable acceleration: Godot `.godot`, Unity `Library`, Unreal `DerivedDataCache` (explicit local DDC environment override; shared DDC disabled). They persist under a project/recipe/toolchain namespace across source generations. Enginebox removes stale source/generated files outside these directories before the next build. A new generation always runs the engine unless it already has a fully verified exact-source artifact. Engine caches are not included in the playable artifact, are not durable source history and must not be evicted while the namespace lock is active. R2 cache snapshots/automatic quota eviction are separate storage policies, not implemented by this CLI.

Fleet jobs must provide a trusted lease file `{attempt:"id",expiresAtMs:123,cancelled:false}`. Standalone local developer builds can omit it. The trusted node atomically replaces this file to renew or cancel; the initial attempt is pinned. Missing, replaced, cancelled or expired leases stop the process group and prevent successful publication. Streaming reads check cancellation at least every 1 MiB and the existing process supervisor checks while the engine runs. Cancellation should update the lease and wait for CLI exit; killing only the CLI first can leave its separately supervised child group holding the inherited lock. Never release native capacity merely because the submitting process disappeared.

Artifacts are staged privately and atomically renamed only after engine success, entrypoint validation, file hashes and fsync. A timeout, cancellation, failed export, absent entrypoint or unsafe output produces no successful artifact receipt. The manifest is `{schema:1,kind:"engine-build",project,generation,sourceTree,recipe,toolchain,entrypoint,files:[{path,sha256,bytes,executable}],totalBytes}`. There are at most 20,000 output files. Artifact ID hashes its canonical JSON; it has no absolute paths or timestamps. Deterministic identification does not claim the engines produce reproducible bytes across independent fresh builds.

The fleet publisher uploads the verified manifest and file chunks, then attaches artifact evidence to the exact generation/candidate. The normal full candidate checks and owner review remain required. Merely exporting a runnable file is not evidence that gameplay, networking, or tests passed.

## Verification

```sh
cargo test --locked -p enginebox-engine-builds
python3 packages/enginebox-engine-builds/tests/godot-proof.py \
  --godot /opt/godot/Godot --templates /opt/godot/templates \
  --out /private/new-owned-proof
pnpm install --frozen-lockfile
node packages/enginebox-engine-builds/tests/browser-proof.mjs /private/new-owned-proof/web-receipt.json
```

The proof directory must not exist. The fixture exports Web and Linux, verifies every output byte, checks artifact ID against independent Python canonical hashing, and runs each build twice to prove exact-artifact reuse. The browser test loads the actual exported WebAssembly game and clicks its button. Run the exported Linux entrypoint in a disposable Linux worker with `--headless -- --enginebox-proof`; it must print `ENGINEBOX_IMMUTABLE_PLAYABLE_OK` and exit zero. Synthetic fixture provenance is explicitly separate from the LayerFS integration proof.

Use the release binary for timing; Rust debug SHA256 loops materially distort cold/cached latency. These small fixtures do not qualify large Unreal project cold starts.

Command references: [Godot CLI](https://docs.godotengine.org/en/stable/tutorials/editor/command_line_tutorial.html), [Godot Web export](https://docs.godotengine.org/en/stable/classes/class_editorexportplatformweb.html), [Unity editor CLI](https://docs.unity3d.com/6000.0/Documentation/Manual/EditorCommandLineArguments.html), [Unreal BuildCookRun](https://dev.epicgames.com/documentation/en-us/unreal-engine/build-operations-cooking-packaging-deploying-and-running-projects-in-unreal-engine).
