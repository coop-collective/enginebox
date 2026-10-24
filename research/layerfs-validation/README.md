# Disposable LayerFS validation

This research directory contains the Linux experiment used for [LayerFS qualification](../../docs/reports/LAYERFS-VALIDATION.md). It is test tooling, not an Engine Box storage provider, production installer or fleet backend. The runtime and native agent homes do not depend on LayerFS.

The pinned upstream revision is `968a5d0be0ea3e9456b06f9148b65523138e9490`. The experiment uses the actual Rust daemon and Node native binding, Docker/FUSE mounts, a real Git repository, TypeScript dependencies, independent file manifests and explicit content assertions. It preserves failed outcomes in JSONL rather than treating a completed campaign as every case passing.

## Isolated machine and transport

Provision a **new disposable Linux VM**, preserving the host's normal admission limits and reserves. The qualified VM had 4 vCPU, 8 GiB RAM, 40 GiB root and 10 GiB state capacity, Ubuntu 24.04, and only its root disk and managed network NIC. Do not use a signed-in workspace, attach personal data, expose the host Docker socket or change host Docker/firewall services. Docker is installed inside this VM only.

Give the fixture a unique `ebox-layerfs-...` name and a unique `user.enginebox.experiment` owner marker during provisioning. Keep its host configuration and all receipts outside Git. For example, a private mode-0600 JSON file has this shape:

```json
{
  "host": "your-test-host-ssh-alias",
  "project": "your-test-project",
  "vm": "ebox-layerfs-YOUR-UNIQUE-ID",
  "owner": "YOUR-UNIQUE-ID",
  "output": "/absolute/private/receipt-directory"
}
```

The optional transport helper takes this file as its first argument:

```sh
node research/layerfs-validation/remote.mjs .state/layerfs-config.json check-config
node research/layerfs-validation/remote.mjs .state/layerfs-config.json inspect
node research/layerfs-validation/remote.mjs .state/layerfs-config.json exec /usr/bin/true
```

It refuses a mismatched experiment marker before guest execution, quotes remote arguments, bounds SSH and remote commands, and writes private receipts. It does not provision, adopt or delete VMs and does not implement a second capacity policy. Use the fleet's existing admission and ownership mechanisms for provisioning and cleanup. Long preparation/build jobs should be launched as scoped services **inside the disposable guest**, then inspected; the transport intentionally limits a foreground remote operation to 180 seconds.

## Build inputs

From a clean upstream checkout at the exact pin, create the source archive:

```sh
git -C .state/layerfs-evaluation/upstream archive \
  968a5d0be0ea3e9456b06f9148b65523138e9490 \
  core/crates core/vendor core/patches core/tools core/Cargo.toml core/Cargo.lock \
  core/benchmark/cluster-two/platform/Cargo.toml \
  core/benchmark/cluster-two/platform/Cargo.lock \
  core/benchmark/cluster-two/passthrough/Cargo.toml \
  core/benchmark/cluster-two/passthrough/Cargo.lock \
  sdk/js .cargo/config.toml LICENSE AGENTS.md \
  > .state/layerfs-evaluation/source.tar
```

The tested archive SHA-256 was `0515f69c92ba1c5a049f3546c61249b355596743f1d7c0d7b39ca060e3eafb4e`. The extra benchmark manifests satisfy upstream workspace references; the retired benchmark is not built or run.

Inside the admitted guest, create `/etc/enginebox-layerfs-validation` as the explicit disposable-machine marker, and a root-owned mode-0700 `/opt/layerfs-validation/`. Extract the archive into its `upstream/` directory. Copy this directory's `.mjs` files into `harness/`, and its Dockerfile and shell scripts into `/opt/layerfs-validation/`. The transport's `push LOCAL GUEST` action only permits destinations beneath this directory.

Run `prepare-guest.sh`, verify it succeeded, then run `build.sh`. The build installs Rust 1.85.1 in a builder container, fetches Cargo's locked dependencies, validates upstream's vendored FUSE provenance and builds the daemon and native addon with `--release --offline --locked`. It records binary hashes and tool versions. The Docker base image is digest-pinned and pnpm is pinned to 12.14.0; apt repositories and installer downloads still require network access, so a later rebuild is not claimed to be byte-identical.

## Campaign

After a successful build, run inside the guest:

```sh
bash /opt/layerfs-validation/run-guest.sh case-YOUR-UNIQUE-ID
```

The run identifier accepts lowercase letters, digits and hyphens; use a new identifier for each changed treatment. Existing output directories are refused. The script creates only its own labeled Docker volumes and containers. A trusted SDK runner receives the **guest's** Docker socket; the two command sandboxes do not. Each measured operation has a 120-second deadline; mounted shell commands have a 100-second timeout and the campaign has a 600-second timeout. Failed/uncertain operations are retained, not automatically replayed.

The synthetic fixture includes a deterministic 16 MiB high-entropy payload, 512 small files, `.git`, installed TypeScript 5.9.3, build outputs and a symlink. It contains no private source or credentials. Manifests compare every regular-file hash and mode plus symlink targets before editing. Manifest audits deliberately run outside fork/mount timings and warm caches before subsequent command measurements. Timings are single diagnostic observations with uncontrolled warm caches, not cold-cache or p95 benchmarks.

Read `receipts.jsonl`, `summary.json`, `run.log`, fixture manifests and retained Docker diagnostics beneath the fresh run directory. Command `PASS` means the operation/content assertion succeeded; it does not mean no packages were downloaded or all application-level cache behavior was preserved. The report interprets reinstall logs, explicit mtime observations and expected capability refusals separately. Store accounting includes main DB, WAL, SHM and live overlays; historical logical bytes are not physical checkout size.

The initial offline reinstall without pnpm's `--frozen-store` failed. Its receipt remains intact. Follow-up native-versus-FUSE dependency controls are separate scripts and receipts, rather than a silent replacement of that result.

After the campaign, run `bash /opt/layerfs-validation/run-dependency-control.sh case-YOUR-UNIQUE-ID control-YOUR-UNIQUE-ID` for the focused cache control. It seeds a new immutable store and metadata cache, runs native ext4 with networking disabled, then checks the equivalent LayerFS install and remount. The FUSE case uses pnpm's offline policy. The tested frozen-store recipe reused the dependency without downloading it; a mount-path metadata change still required an explicit Commit before unmount. Never update the published store while frozen-store readers use it. This is a single-package fixture, not qualification of package lifecycle scripts or native build caches.

Export receipts before cleanup. Inspect the exact VM's experiment owner marker, stop only task-owned workloads and remove that disposable VM through the operator's fleet tools. Never use broad Docker pruning or delete resources by a prefix alone. The October 19 qualification also exercised the public transport's inspect/exec, wrong-owner refusal and literal shell-argument handling against the owned fixture.
