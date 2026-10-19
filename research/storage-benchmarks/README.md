# Storage qualification

`lore.mjs` measures a private Unity content copy against an owned loopback Lore server. It does **not** measure Cloudflare Artifacts, R2 or GitHub. See [results and storage recommendation](../../docs/LORE-STORAGE.md).

Use Node 22.12+ and the verified native Lore **v0.10.1** CLI and server release for your platform. Extract both binaries into one directory, check their release SHA-256 digests, and make them executable. No server or CLI is installed by this script.

```sh
node research/storage-benchmarks/lore.mjs \
  /path/to/unity-project \
  .state/lore/fresh-run \
  /path/to/verified-lore-binaries
```

The output must not exist and must be outside the source. `Assets`, `Packages` and `ProjectSettings` are copied; symlinks and special files are refused. The workload requires a TIFF larger than 1 MiB and `ProjectSettings/ProjectVersion.txt`. It changes 4 KiB of that TIFF and appends a comment to the private settings copy. These deliberately artificial edits test storage, not Unity asset validity. The original tree is checked again at the end.

The fixture must be below 20 GiB. Allow six times its content size plus 4 GiB free disk; do not depend on filesystem cloning to satisfy capacity. Ports 43187 (TCP/UDP) and 43189 (TCP) must be free. The server binds loopback only and is stopped in the finalizer. Each command has a 15-minute timeout. No source content is uploaded to an external service.

`report.json` contains measured wall times, exact content counts and summed lengths of store files (not allocated blocks). SHA-256 checks validate both complete clones and the incremental sync. Timings exclude these verification passes. Commands use four Lore worker threads and `--sync-data`. The second checkout uses the same explicitly selected shared store with `--cache`; OS page caches are not flushed. Online Lore commit uploads content, so include **commit plus push** when reporting initial publication.

Private logs, server data and working copies remain under the output for review; they may contain project paths and contents and must not be committed. `LORE_GLOBAL_PATH` isolates the registry and configuration under that output as well. The shared store is explicitly selected, with `--make-default false`. After inspection, remove only the run's owned output if desired. No global cleanup is performed.

This is a manually invoked qualification tool, not a CI performance assertion. One observation cannot establish latency percentiles, remote throughput, production durability, engine build correctness or results for a much larger Unreal project.
