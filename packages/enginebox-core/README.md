# Engine Box core

Native Rust fleet control, Incus lifecycle/snapshots, conservative capacity admission, template desired-state registry, prebooted workspace reserves, Linux guest preparation, and portable read-only host inventory. Provider authentication/session code is in `../enginebox-agents`; this crate never implements inference or copies account homes into reusable bases.

`Fleet::load(config, state_root)` reads the versioned fleet TOML and private controller registry. It does not start background work. Controllers call `reconcile()` only while active and install lifecycle guards/post-start credential synchronization hooks. All host mutations take the same root-owned host flock used by bootstrap; controller jobs additionally serialize by host.

## Operator entry points

```sh
enginebox-host --json --docker --cpus 4 --memory-gib 8
enginebox-host bootstrap init --output host.toml
enginebox-host bootstrap doctor --config host.toml
enginebox-host bootstrap plan --config host.toml
enginebox-host bootstrap apply --config host.toml
```

The generated configuration must be filled with an explicit host, storage pool and network. Inspection and apply need noninteractive administrator access. Linux x86_64 with KVM, Incus 6+, QEMU 8.2+, and Ubuntu 24.04 guests are the qualified backend. Automatic host package installation is opt-in and limited to Ubuntu 24.04/26.04. Mac and Windows builds provide controller/operator tools and hardware/Docker capability inspection; native Mac/Windows guests, GPU scheduling and Docker workspace enrollment remain separate unimplemented backends.

## Migration from v0.2

Registry manifest digests, host-binding receipts, bootstrap recipe fingerprints and warm-generation hashes preserve the released format, including its differing Unicode canonicalization rules. The regression fixture is fabricated data captured from the v0.2 implementations and is independent of the old source tree.

Install the matching Linux release on each node before switching controllers. The host executable must be available through `sudo`'s executable search path, and its sibling release directory must contain `enginebox-host`, `enginebox-guest`, and `enginebox-mcp`. A macOS/Windows controller sends configuration over SSH; only binaries already installed on the Linux node are copied into Linux guests.

For an already running assigned workspace, the explicit helper migration is:

```sh
sudo enginebox-host sync-guest-tools --project PROJECT --name ebox-example
```

Replace the example with the real lowercase workspace name. The command authorizes the running Engine Box VM and its ownership before changing files. It validates the entire source release's Linux ELF architecture and ownership first, rejects symlink/foreign destination binaries, verifies staged SHA256, and replaces each helper atomically. Existing processes retain their open executable inode. It changes no provider CLI, home, credentials, memory, identity, service profile or running session.

Create/resume and warm assignment check helper versions before readiness. A legacy warm reserve is durably claimed, resumed, then upgraded before it becomes ready for agent access; failed startup never returns that VM to the pool. All three helper versions are checked so an interrupted rollout can resume. The `Fleet::sync_guest_tools` method provides the same controlled migration for running assigned VMs.

Clean templates for the Rust provisioning recipe use manifest revision 4. Existing revision 3 digests are not silently rewritten. Helper-only migration preserves an old guest's original identity service; installing a new revision 4 clean template is what replaces Python-based guest preparation and identity units. Never run full bootstrap preparation against an assigned workspace with account state.

## Validation

`cargo test -p enginebox-core` exercises admission across all projects, contribution ceilings, durable warm claims/failure quarantine, private registry state, SSRF-safe imports, strict duplicate JSON decoding, recipe/receipt compatibility, inspect-first bootstrap planning, helper migration ownership, and process timeouts. Executable Rust Incus fixtures run actual transport operations with isolated subprocess environments, without SSH or a real VM.

`cargo clippy -p enginebox-core --all-targets -- -D warnings` checks the crate. Real Linux Incus provisioning and provider execution must be qualified on a disposable VM separately before release. Unit tests do not establish Windows/macOS native VM support or instantaneous resume timing.

The ignored `live_qualification` test requires explicit live-test opt-in, an absolute private fleet configuration path, a host ID, a Linux candidate binary directory on that host, and an owned clean source snapshot:

```sh
ENGINEBOX_LIVE_QUALIFICATION=1 \
ENGINEBOX_TEST_FLEET=/absolute/private/fleet.toml \
ENGINEBOX_TEST_HOST=primary \
ENGINEBOX_TEST_RELEASE_BIN=/absolute/staged/linux/bin \
ENGINEBOX_TEST_SOURCE=ebox-base/prepared-example \
cargo test -p enginebox-core --test live_qualification -- --ignored --nocapture
```

It creates a uniquely marked clone only after actual fleet admission under the host lease. Its preparing status excludes controller credential delegation. It checks native helper versions and SHA256, readiness and idle leases, guest MCP isolation, real headless browser navigation, and pause/resume; then deletes only its marked clone and temporary staging directory. `ENGINEBOX_TEST_REPORT` accepts an absolute private JSON report path. `ENGINEBOX_TEST_PREPARE=1` also runs the full Rust preparation path on the clean clone and reboots it. This option qualifies preparation with already installed native clients; a fresh Ubuntu installation and authenticated inference still require separate qualification.
