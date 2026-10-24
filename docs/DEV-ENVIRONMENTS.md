# Persistent development environments and agent forks

Engine Box's next application-environment workflow has two lifetimes: a persistent **core environment** for the project, and disposable **agent environments** for individual tasks. The core follows the project's main branch and keeps its application data as code changes. An agent starts from a published revision of that environment, makes changes against its own data, and submits code for review.

The broader fork/publication workflow below remains a product contract. The optional [environment service](ENVIRONMENT-SERVICES.md) now implements a protected registry, exact-source following, bounded application containers, persistent data, readiness, recovery and dashboard-managed HTTP/WebSocket/TCP/UDP routes. Git main has executable end-to-end qualification; the accepted LayerFS export adapter has provenance/byte-integrity contract tests and still needs live qualification with the separate integration executor. Consistent online data publication and disposable data forks remain unimplemented. No template field is defined by this document.

## One persistent core per project

The core records its repository, followed branch (normally `main`), last applied commit, runtime/toolchain revision, application configuration and persistent data identity. Updating code must not replace the data directory with a clean template. Apply the project's migrations explicitly, check readiness, and record which code/data schema combination is running. A failed update leaves a visible deployment outcome; rolling code back does not automatically undo a database migration.

A project may configure an optional dev start command, such as its existing `pnpm dev` script, together with working directory, readiness check and exposed service ports. That command belongs to the proposed application service, not today's template manifest. Repositories without a server can leave it unset. The service should supervise only the process group it owns and expose startup failure and logs.

Sleep and wake are application-environment operations. Sleeping releases the selected compute while retaining the project's files and persistent data. Waking starts or resumes the environment, runs the configured command when needed and checks the application before publishing its endpoint as ready. A sleep policy must say whether it retains RAM or restarts processes. Current Incus pause keeps RAM allocated; clean stop releases compute and boots again on resume. LayerFS-attached VMs support pause/resume, but coordinated RAM hibernation remains unqualified.

## Publish from the online core

The core is a long-lived service. Publishing a baseline must not require turning it into a disposable stopped workspace. The publication adapter needs database-aware capture, a bounded application write barrier, or a combination appropriate to the configured services. A brief barrier may be needed to align several databases, object metadata and background workers; a filesystem copy by itself does not supply that consistency.

The proposed publication sequence is:

1. Identify the running code commit, toolchain, binding identities, schema/migration versions and the data services included in the baseline.
2. Capture a consistent point using the adapter's backup protocol and any required brief write barrier. Account for in-flight requests, background writers and externally visible effects. Release the barrier once the adapter has secured that point; later materialization and validation must not require holding live application writes unnecessarily.
3. Materialize an immutable candidate revision outside the core's live writable data. Verify the captured state and prepare the files/dependencies needed by new environments.
4. Publish the manifest only when every component is complete. Retain the previously published revision if capture or validation fails, while the core continues serving normally.
5. Create each agent environment from that exact published revision, with its own writable data, endpoint and native agent home. Run application readiness before handing it to a task.

These are design steps, not currently callable Engine Box operations. Publication must report its actual interruption/consistency guarantees; “online” does not promise zero write pause. A version of SQLite's backup API can capture one database while it is in use, but that does not establish a consistent application-wide point across separate bindings. Copying a main SQLite file without its active WAL is not a backup protocol. [SQLite backup](https://www.sqlite.org/backup.html), [SQLite WAL](https://www.sqlite.org/wal.html)

## What an agent inherits

| State | Ownership |
| --- | --- |
| Code, prepared dependencies and published seed data | Immutable revision available to authorized project forks |
| Live local databases, objects and generated task files | Private writable state for one agent environment |
| Core application's current data | Persistent core service; never the agent's writable mount |
| Intentionally shared project services or notes | Explicit project-scoped APIs and access policy |
| Provider credentials, chat history, personal skills and browser profiles | Private owner/workspace state, excluded from reusable baselines |

Two environments must not point independent SQLite/Miniflare runtimes at the same writable persistence directory. Fork isolation comes from independent writable state, not from distinct ports or branch names. Sharing a service is a different choice: agents call its API with the intended permissions and accept that writes affect shared state.

Later changes in the core do not silently appear in an existing fork. Refresh creates a new fork or performs a deliberate application-specific import. Code returns through reviewed LayerFS branch integration. An optional GitHub connection exports accepted main revisions and uses a specialized Git environment for external conflicts; see [GitHub sync](GITHUB-SYNC.md). Data returns only through an explicit migration, export/import or application operation; there is no automatic database merge. Task completion can discard the fork without deleting the core or its published baseline.

## Cloudflare as the first recipe

The first proposed recipe runs Wrangler and local bindings in the environment. D1, KV, R2 and SQLite Durable Object state use an explicit persistence path; connected Workers start together with their service bindings. Code updates and local state have separate lifecycles. DuckDB is not a dependency.

The initial supported set should be local bindings with recorded Wrangler/Miniflare/workerd versions. Remote bindings remain external services and are not captured by a local data revision. Queues, alarms, scheduled work, open sockets and third-party calls need explicit capture/restart rules before support is advertised.

Keep authoritative core data on storage with a qualified persistence and backup contract. LayerFS is useful for prepared checkouts, dependencies and disposable agent data forks, but its pinned preview does not establish production power-loss durability. Its current Incus tests do not qualify workerd or SQLite WAL/locking behavior over that mount. See [LayerFS scope](LAYERFS.md) and [attachment qualification](LAYERFS-INCUS.md).

## Existing local proof

An earlier executable experiment (not shipped in this repository) tested local Wrangler data forks.

On October 18, 2026, all 15 checks passed on macOS ARM64 with Wrangler 4.151.1, Miniflare 5.20261012.1-beta and workerd 1.20261012.1. The probe started two connected Workers per environment, stopped the parent before an ordinary full-directory copy, then ran the parent and two forks concurrently. Local D1, KV, R2, SQLite Durable Object data and the auxiliary Worker's D1 inherited the seed, diverged independently and survived a Wrangler restart. The saved checkpoint remained byte-identical. A separate native DuckDB file was also tested historically; it is not part of this recipe.

The fixture contained about 1.7 MB and used ordinary copies, not LayerFS, Cloudflare snapshots or copy-on-write storage. It proves local stopped-state inheritance and isolation at those versions. It does not prove online core publication, main-branch updates, hosted lifecycle, crash recovery, production cloning or large-data performance.

## Delivery gates

Build the project environment registry and owned startup service first, including code-update/data-preservation tests and observable sleep/wake behavior. Then qualify a Cloudflare publication adapter with concurrent writes, complete binding state, version compatibility and failure during publication. Finally run two agent forks against the published revision, verify isolation and restart, and measure application readiness and storage cost at representative data sizes. Promote the workflow into the dashboard/API/MCP only after those contracts are exercised end to end.
