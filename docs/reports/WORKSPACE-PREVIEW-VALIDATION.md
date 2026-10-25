# Open-workspace preview qualification

The command-boundary LayerFS exporter passed a disposable Linux ARM64 Docker
qualification on October 25, 2026. This is a real native LayerFS Store/FUSE test,
separate from the environment supervisor's simulated-Docker unit tests. It is not
native macOS filesystem support or attached-Incus live projection.

Tested: pinned LayerFS revision `968a5d0be0ea3e9456b06f9148b65523138e9490`,
Node 24.22.0, SDK build Rust 1.85.1, Enginebox build Rust 1.96.0, and Docker
29.3.0/API 1.54. Store and SDK metadata used new Linux ext4 named volumes. A
trusted disposable operator container shared the local Docker socket and used
read-only daemon-root visibility for the ordinary filesystem preflight. The
owned runtime image configured the SDK's documented
`LAYERFS_DAEMON_HOST=host.docker.internal` for Docker Desktop's published loopback
ports. No existing Store was opened or developer workspace stopped.

The initial isolated operator container was refused by the ext4 preflight because
its view of DockerRootDir was overlayfs. After correcting filesystem visibility,
ordinary loopback dialing failed through Docker Desktop. Configuring the SDK's
supported Docker Desktop host fixed that topology. Neither failure was treated as
a passing qualification or bypassed in product code.

The executable proof is
[`live-workspace-previews.mjs`](../../packages/enginebox-storage-layerfs/tests/live-workspace-previews.mjs).
Run on a disposable Linux node with the pinned SDK/image and a new private config:

```sh
ENGINEBOX_DISPOSABLE_PROOF=1 node \
  packages/enginebox-storage-layerfs/tests/live-workspace-previews.mjs \
  /absolute/ebox /private/fresh-config.json /private/fresh-evidence.json
```

The config's root and evidence path must not exist. The proof imports a synthetic
credential-free project, creates two open workspaces, and retains its owned
resources for evidence and explicit cleanup. Observed:

- Independent draft contents and unchanged accepted main.
- Stable revision/source reuse for an unchanged open workspace.
- Cross-project export and old closed-exporter use rejected.
- Export rejected while a real writer held the workspace lease between two file
  writes; after completion both new file versions appeared in one immutable batch.
- Rename/removal represented by the new manifest while the prior snapshot stayed
  immutable.
- A synthetic `.env` path caused export refusal without altering the prior snapshot.
- The workspace remained ready with no checkpoint after all exports.
- A trusted local transport copied the immutable Linux export into a local staging
  directory while preserving its manifest; the consumer verified source provenance.

The environment-service test suite passed 30 tests, including six preview tests
covering lifecycle, independent environments, stale and concurrent lease mutations,
source project/workspace/sandbox/branch fencing, source rename/removal, failed
preparation, restart, expired in-flight source retrieval, ownership-checked cleanup,
authenticated API reads/mutations, hidden lease tokens in the general dashboard
catalog, accepted-main compatibility, and explicit ingress ports. These use a
simulated Docker runner. Targeted storage unit tests passed 29 tests with one
unrelated GitHub network test ignored; targeted Rust clippy passed.

Browser/Vite HMR and application ingress have a separate GameJam integration
qualification. A passing file-export test alone does not establish WebSocket
admission, rendered scene readiness, attached-guest coherence, or streaming of
individual saves during a long-running native command.

Repository gates also passed: `pnpm install --frozen-lockfile`, `pnpm check`,
`pnpm test`, `pnpm lint:rust`, and `pnpm build`. A final environment-service test
run after lease shutdown fencing passed all 30 tests. The fleet build used
Wrangler's dry-run mode; no production service or Worker was deployed.
