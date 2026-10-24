const escape = (value) => value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
const code = (value) => `<pre><code>${escape(value)}</code></pre>`;
const source = (path, label) => `<a href="https://github.com/coop-collective/enginebox/blob/main/${path}">${label}</a>`;

export const guides = [
  {
    slug: 'introduction',
    title: 'Introduction',
    group: 'Start here',
    summary: 'Run native coding agents on your own machines, with prepared workspaces and private persistent state.',
    sections: [
      {
        id: 'what-engine-box-manages',
        title: 'Your machines, the original clients',
        html: `<p>Engine Box prepares Linux workspaces and runs the official Codex, Claude Code and Cursor Agent clients inside them. A controller manages your hosts, templates, account connections and workspace lifecycle through a dashboard, HTTP API and MCP server.</p><p>The clients keep their own agent loops, tools and provider connections. Engine Box supplies the environment around them. Provider plan eligibility, usage limits and charges still apply.</p>`,
      },
      {
        id: 'three-parts',
        title: 'Controller, nodes and workspaces',
        html: `<ul><li><strong>Controller:</strong> your fleet inventory, template registry, private account connections and operation queue.</li><li><strong>Nodes:</strong> Linux machines with Incus/KVM, explicit resource limits and prepared templates.</li><li><strong>Workspaces:</strong> private VMs with a native agent home, project files and optional browser tools.</li></ul><p>A credential-free template or unused warm reserve is reusable. Once a workspace is assigned, its home, files and checkpoints are private. It never returns to the fresh reserve pool.</p>`,
      },
      {
        id: 'reuse',
        title: 'Prepare once, reuse deliberately',
        html: `<p>Templates carry installed clients and tools. Warm pools keep never-used VMs ready to assign. The first-class LayerFS backend adds approved checkout imports, immutable dependency reuse and private writable file branches on qualified Linux nodes.</p><p>Compute, project files and native agent state have different lifetimes. A file checkpoint preserves files; it does not preserve a process's RAM or a live network connection. Read <a href="/docs/storage/">Storage</a> and <a href="/docs/agent-state/">Agent state</a> before choosing a reuse policy.</p>`,
      },
      {
        id: 'next-workflow',
        title: 'A persistent project, disposable agent forks',
        html: `<p>The next application workflow keeps one persistent core environment following your main branch. Its data survives code updates, and an optional dev command starts the application. The core publishes consistent data revisions for disposable agent environments with private writable state.</p><p>This managed service is planned. The current workspaces and storage are its foundation. See the <a href="/docs/dev-environments/">environment design</a> and <a href="/docs/platform-support/">availability matrix</a> for the boundary.</p>`,
      },
      {
        id: 'self-hosting-license',
        title: 'Self-hosting and the license',
        html: `<p>Engine Box is source-available under the Sustainable Use License. Personal and internal business self-hosting are permitted by its terms. Offering commercial hosted access or selling the software requires a separate agreement. This is not an OSI open-source license.</p><p>Read the ${source('LICENSE', 'license')} for the exact terms, then start with the <a href="/docs/quickstart/">source quickstart</a>.</p>`,
      },
    ],
  },
  {
    slug: 'quickstart',
    title: 'Quickstart',
    group: 'Start here',
    summary: 'Build the current source, inspect a Linux host and start the controller.',
    sections: [
      {
        id: 'requirements',
        title: 'Choose your operator and compute machines',
        html: `<p>Use Node.js 22.12 or newer and the repository's pinned pnpm 12.14.0. The root workspace provisions Rust 1.96.0 through pnpm's Cargo integration. Python is not a runtime dependency.</p><p>Compute workspaces currently require a prepared Linux Incus/KVM host. A macOS or Windows operator can manage a separate Linux node; installing the controller does not contribute that laptop's CPU or GPU to the fleet. Check <a href="/docs/platform-support/">platform support</a> before provisioning.</p>`,
      },
      {
        id: 'build',
        title: 'Build the source',
        html: code(`git clone https://github.com/coop-collective/enginebox.git
cd enginebox
pnpm install --frozen-lockfile
pnpm build
target/release/ebox --help`) + `<p>The native binaries are in <code>target/release</code>; the dashboard is built from <code>packages/dashboard</code>. Both <code>Cargo.lock</code> and <code>pnpm-lock.yaml</code> are part of the reproducible workspace.</p><p>Release installers are version-pinned. Use an archive only when that exact version and platform have published, qualified assets; the source version alone does not establish availability. See ${source('docs/INSTALL.md', 'installation details')}.</p>`,
      },
      {
        id: 'prepare-node',
        title: 'Inspect before provisioning',
        html: code(`target/release/ebox init --output enginebox.toml
# Edit the generated host, storage and network configuration.
target/release/ebox doctor --config enginebox.toml
target/release/ebox plan --config enginebox.toml
# Review the plan before applying it.
target/release/ebox apply --config enginebox.toml`) + `<p>Configure the intended Linux host and owned Incus resources. Bootstrap prepares a credential-free base and reports its snapshot. Existing storage pools, unrelated profiles and firewall rules are not permission to overwrite them. Follow ${source('docs/BOOTSTRAP.md', 'the bootstrap guide')} for host prerequisites and preparation evidence.</p>`,
      },
      {
        id: 'controller',
        title: 'Start your controller',
        html: `<p>Create a private <code>fleet.toml</code> from ${source('examples/fleet.example.toml', 'the fleet example')}. Replace the sample host, project, pool and prepared snapshot with your inspected values. Then create an API token and start the controller:</p>` + code(`umask 077
mkdir -p "$HOME/.local/state/enginebox"
(set -C; openssl rand -hex 32 > "$HOME/.local/state/enginebox/api.token")
export ENGINEBOX_API_TOKEN_FILE="$HOME/.local/state/enginebox/api.token"
target/release/enginebox-controller --config fleet.toml`) + `<p>The token command refuses to overwrite an existing file. The API listens on <code>127.0.0.1:8765</code>. Keep the token, controller state and host inventory outside Git. Use an SSH tunnel or your own authenticated HTTPS gateway for remote access.</p><p>For a persistent service with the dashboard, follow ${source('docs/INSTALL.md#configure-a-controller', 'controller installation')}; it includes the owner password, stable session secret and supervisor configuration.</p>`,
      },
      {
        id: 'first-workspace',
        title: 'Connect an agent and allocate a workspace',
        html: `<ol><li>Register a <a href="/docs/templates/">template</a> and wait for its revision to be installed on your node.</li><li>Connect the owner account in the dashboard's <strong>Agents</strong> panel.</li><li>Allocate a workspace using Minimum, Recommended or custom resources.</li><li>Wait for readiness, then use its native terminal or <a href="/docs/mcp-api/">fleet MCP</a>.</li></ol><p>A ready VM proves the expected tools and networking are present. A successful provider prompt is a separate check of authentication, subscription eligibility and remaining allowance.</p>`,
      },
    ],
  },
  {
    slug: 'dev-environments',
    title: 'Development environments',
    group: 'Workspaces',
    summary: 'The planned workflow: a persistent core environment and private data forks for agent tasks.',
    sections: [
      {
        id: 'core',
        title: 'Keep a core environment for the project',
        html: `<p><strong>Planned workflow.</strong> Each project has a persistent core environment following a branch, normally <code>main</code>. It records the applied code revision, runtime, configuration and data identity. Updating code preserves its application data and runs explicit migrations before the new version becomes ready.</p><p>The core can be your development or staging service. It is not an agent's disposable scratch space. Rolling back code does not automatically reverse a database migration.</p>`,
      },
      {
        id: 'start-and-sleep',
        title: 'An optional start command, with sleep and wake',
        html: `<p>A project will be able to declare its existing dev command, working directory, readiness check and exposed ports. For example, a repository may already use <code>pnpm dev</code>. Projects without a server can leave the command unset.</p><p>The managed service will retain files and application data when sleeping, then start or resume compute and check readiness on wake. The policy must distinguish retaining process RAM from restarting processes. These application settings are planned; they are not fields accepted by today's template manifest.</p><p>Current VM pause/resume is available. Pause keeps RAM allocated. A clean stop releases compute and requires a boot. LayerFS-attached VMs do not yet support coordinated RAM hibernation.</p>`,
      },
      {
        id: 'publish',
        title: 'Publish a consistent revision from the online core',
        html: `<ol><li>Identify the code, schema, bindings and runtime versions to capture.</li><li>Capture application data with a database-aware adapter and any required brief write barrier.</li><li>Prepare and validate an immutable candidate outside the core's live writable data.</li><li>Publish its versioned manifest only after every component succeeds.</li></ol><p>The core remains the persistent service. “Online” does not promise zero write interruption: multiple databases and background writers may need a short coordinated barrier. An interrupted publication must leave the previous revision available.</p><p>Filesystem branching is one part of this design. Copying a live SQLite file alone does not capture its WAL or establish consistency across services.</p>`,
      },
      {
        id: 'fork',
        title: 'Give every task its own writable data',
        html: `<p>An agent environment starts from an exact published revision, reusing prepared code and dependencies. Its databases, generated files, endpoint and native agent home are private writable state. Two independent runtimes must not share the same writable SQLite persistence directory.</p><p>Later core changes do not silently appear in an existing fork. Refresh deliberately by creating a new fork or using an application-specific import. Return code through Git review; return data through explicit migrations or imports. There is no automatic database merge.</p><p>Shared project services remain possible through explicit APIs and permissions. A shared service and an isolated data fork have different semantics.</p>`,
      },
      {
        id: 'delivery',
        title: 'What exists and what comes next',
        html: `<p>Native workspaces, templates, VM lifecycle and LayerFS checkout branching are implemented. An earlier local Wrangler experiment proved seed inheritance, independent writes and restart across two data forks.</p><p>The core registry, main-branch reconciliation, owned application startup, sleep/wake endpoint lifecycle and online publication adapters remain planned. Cloudflare local bindings are the first intended recipe. Read <a href="/docs/cloudflare/">the Cloudflare guide</a> or ${source('docs/DEV-ENVIRONMENTS.md', 'the full architecture contract')}.</p>`,
      },
    ],
  },
  {
    slug: 'agent-state',
    title: 'Agents and private state',
    group: 'Workspaces',
    summary: 'Connect once, launch official clients and keep each workspace’s conversations and memory.',
    sections: [
      {
        id: 'connections',
        title: 'Accounts belong to the controller',
        html: `<p>Connect Codex, Claude Code and Cursor Agent through the dashboard's <strong>Agents</strong> panel. Native sign-in runs on the controller. Refresh credentials stay in its private provider homes; assigned workspaces receive compatible access material for the same owner.</p><p>Prepared bases and unused warm reserves remain credential-free. A sleeping workspace catches up when it resumes. Connecting an account does not multiply its allowance or guarantee every model is included in its plan.</p>`,
      },
      {
        id: 'homes',
        title: 'Keep the whole native home',
        html: `<p>Engine Box preserves each workspace's private home between runs, including native settings, skills, memory, transcripts and browser profiles. Project files also retain repository instructions, scratch notes and build output.</p><table><thead><tr><th>Client</th><th>Examples of private state</th></tr></thead><tbody><tr><td>Codex</td><td><code>~/.codex</code>, <code>~/.agents</code>, project instructions</td></tr><tr><td>Claude Code</td><td><code>~/.claude</code>, <code>~/.claude.json</code>, project memory</td></tr><tr><td>Cursor Agent</td><td><code>~/.cursor</code>, <code>~/.config/cursor</code>, project configuration</td></tr></tbody></table><p>Stable project paths matter to native memory lookup. Engine Box does not merge or synchronize whole homes between workspaces. Portable tenant state and cross-workspace memory are separate future capabilities.</p>`,
      },
      {
        id: 'launch-and-resume',
        title: 'Launch and resume an exact conversation',
        html: `<p>Call the operator MCP tool <code>agent_launch</code> for an assigned, running workspace:</p>` + code(`{
  "host_id": "primary",
  "name": "ebox-example",
  "provider": "claude",
  "prompt": "Read this project's instructions and summarize its structure.",
  "workdir": ".",
  "timeout_seconds": 120
}`) + `<p>Poll <code>agent_run</code> with the returned run ID and the same host/workspace. To continue that conversation, call <code>agent_launch</code> again with <code>resume_id</code> set to its returned <code>native_session_id</code>. Engine Box requires the exact native UUID; it does not guess the latest chat.</p><p>The clients run under their native tool and approval policies. The bounded API currently allows runs up to 300 seconds with at most 256 KiB of combined output. Direct terminals remain available for interactive workflows. See ${source('docs/TENANT-AGENT-STATE.md', 'native run semantics')}.</p>`,
      },
      {
        id: 'allowance',
        title: 'Usage is an observation, not a shared balance',
        html: `<p>The Agents panel reports native allowance data when a qualified provider interface exposes it. Otherwise it shows that usage is unavailable and links to the provider. An allowance reset is not automatically a billing renewal date.</p><p>Parallel work still consumes the connected account's limits. Engine Box does not pool unrelated accounts or share provider-side inference caches. Native conversation resume restores saved context; VM memory preservation and provider prompt caching are different mechanisms.</p>`,
      },
      {
        id: 'ownership',
        title: 'One trusted owner today',
        html: `<p>The current controller serves one owner. Its operator token grants fleet authority and must stay outside guest environments. Private homes, credentials and checkpoints must never become reusable templates.</p><p>Parallel agents can use separate private workspaces. Public multi-tenant hosting, movable tenant volumes and shared writable native homes are not part of the current authorization model. See <a href="/docs/platform-support/">availability and limits</a>.</p>`,
      },
    ],
  },
  {
    slug: 'cloudflare',
    title: 'Cloudflare development',
    group: 'Workspaces',
    summary: 'A local Wrangler recipe is the first target for persistent core environments and isolated data forks.',
    sections: [
      {
        id: 'recipe',
        title: 'Local bindings, persistent project data',
        html: `<p><strong>Planned managed recipe.</strong> The core environment runs your Worker project through Wrangler, follows main and retains local application state between code updates. Its optional dev command starts the application; sleep and wake retain that state.</p><p>Local D1, KV, R2 and SQLite Durable Objects are the initial data targets. Connected Workers need their service bindings and startup configuration captured together. DuckDB is not required.</p><p>The current template catalog installs native development tools. It does not yet provide this managed Cloudflare environment or a main-tracking staging service.</p>`,
      },
      {
        id: 'persistence',
        title: 'Use one persistence directory per environment',
        html: `<p>Wrangler's local state normally lives under <code>.wrangler/state</code>; <code>--persist-to</code> selects an explicit location. The environment's runtime and local data-management commands must agree on that location. Pin the Wrangler, Miniflare and workerd versions used to create and consume a revision.</p><p>Every fork gets private writable state. Separate ports or Git branches do not isolate two runtimes pointed at the same database files. Remote bindings remain external services; a local filesystem snapshot does not clone them.</p><p>For current Wrangler behavior, consult <a href="https://developers.cloudflare.com/workers/local-development/local-data/">Cloudflare's local-data documentation</a>.</p>`,
      },
      {
        id: 'existing-proof',
        title: 'The earlier prototype did exercise data forks',
        html: `<p>A local experiment passed 15 checks on October 18, 2026. It ran two connected Workers per environment, copied a stopped parent's state, then ran the parent and two forks concurrently. Seeded D1, KV, R2, SQLite Durable Object and auxiliary Worker D1 data diverged independently and survived restart.</p><p>That test used an approximately 1.7 MB fixture and ordinary directory copies on macOS, with Wrangler 4.151.1, Miniflare 5.20261012.1-beta and workerd 1.20261012.1. It was not a LayerFS benchmark or an online snapshot implementation.</p><p>${source('docs/DEV-ENVIRONMENTS.md#existing-local-proof', 'The evidence record')} identifies the adjacent research checkout and its saved results. It supplies a useful starting point for integration.</p>`,
      },
      {
        id: 'online-publication',
        title: 'The next step is consistent online publication',
        html: `<p>The managed core should publish an immutable data revision while remaining a persistent service. A database-aware adapter must capture complete state and use a brief write barrier where several bindings need one consistent application point. Publication must fail safely without replacing the previous baseline.</p><p>Agent forks can then reuse that revision while keeping their own live data. Code returns through review; data changes require explicit migrations or imports. Queues, alarms, scheduled work and external side effects need their own restart and capture contracts.</p><p>LayerFS checkout tests do not yet qualify workerd, SQLite WAL/locking or database recovery over its Incus mount. Authoritative core data stays on separately qualified persistent storage. See <a href="/docs/dev-environments/">the environment design</a> and <a href="/docs/storage/">storage scope</a>.</p>`,
      },
    ],
  },
  {
    slug: 'storage',
    title: 'Checkouts and storage',
    group: 'Workspaces',
    summary: 'Reuse approved source and packages while every agent keeps a private writable view.',
    sections: [
      {
        id: 'layers',
        title: 'Separate reusable files from private state',
        html: `<p>LayerFS is a first-class Engine Box checkout backend, exposed through the controller, Storage dashboard and MCP. Qualified nodes import an approved repository revision, prepare shared immutable dependencies and create independent writable branches at a stable <code>/project</code> path.</p><p>Native agent homes, provider credentials, browser profiles and private memory stay outside reusable checkout bases. Import approval is explicit: tracked source can contain secrets too.</p>`,
      },
      {
        id: 'workflow',
        title: 'Preview, import, fork',
        html: `<ol><li>An administrator provisions the pinned LayerFS runtime and registers fixed node configurations and repository aliases.</li><li>Use <code>storage_preview</code> to inspect the exact source revision and obtain its approval digest.</li><li>Use <code>storage_import</code> with that revision, digest and an explicit credential-free assertion.</li><li>Use <code>storage_fork</code> for a private writable workspace.</li><li>Use <code>storage_action</code> to prepare dependencies, checkpoint, remount, close or reconcile supported interrupted operations.</li></ol><p>These operations return durable jobs. Poll completion before using the result. Requests select registered IDs; they do not provide arbitrary host paths or shell commands. See ${source('docs/LAYERFS-BACKEND.md', 'backend configuration and operation reference')}.</p>`,
      },
      {
        id: 'dependencies',
        title: 'Reuse a prepared dependency generation',
        html: `<p>A published package cache is immutable. The qualified pnpm recipe prepares cache metadata before publication, uses an offline frozen install and does not run package lifecycle scripts. Create a new generation to add packages instead of modifying a store that active agents share.</p><p>Keep project paths stable so generated shims and native memory resolve consistently. Large Git LFS assets, submodule hydration, incremental builds and game-engine workloads still need separate qualification; a small checkout test does not establish their performance.</p>`,
      },
      {
        id: 'incus',
        title: 'Attach a checkout to a VM explicitly',
        html: `<p>The node-administrator Incus adapter exports a selected checkout into an owned, stopped VM. The qualified path supports guest writes, script and ELF execution, shared writable mmap, pause/resume and clean detach/reopen. Fleet allocation does not attach it automatically.</p><p>Attachment enforces one writer. Storage operations that would change or close the branch are blocked while the VM owns it. Attached VMs cannot currently hibernate, snapshot or fork; file and RAM checkpoints need a coordinated consistency contract.</p><p>See ${source('docs/LAYERFS-INCUS.md', 'the attachment contract')} before enabling a node.</p>`,
      },
      {
        id: 'recovery',
        title: 'Checkpoints and recovery have explicit boundaries',
        html: `<p>A file checkpoint records a file revision. It does not capture active processes, sockets or guest RAM. Recovery verifies operation receipts and live owned resources; it does not silently replay commands or invent a successful outcome.</p><p>The pinned preview has no qualified power-loss durability guarantee. Keep irreplaceable state and authoritative application databases on separately qualified persistent storage. The current recovery and Incus tests do not establish live Wrangler database capture.</p><p>See ${source('docs/reports/LAYERFS-RECOVERY-VALIDATION.md', 'recovery qualification')} and <a href="/docs/platform-support/">remaining availability gates</a>.</p>`,
      },
    ],
  },
  {
    slug: 'templates',
    title: 'Templates and warm pools',
    group: 'Operate',
    summary: 'Register a portable template once and synchronize it across your fleet.',
    sections: [
      {
        id: 'registry',
        title: 'The controller owns desired state',
        html: `<p>Register a template in the controller. It synchronizes the registered revision to configured nodes and records each node's installation outcome. Newly configured nodes join reconciliation after the controller reloads its host configuration.</p><p>Templates are separate from the Engine Box installation. A manifest defines its supported recipe, source image, tools and minimum/recommended resources. Existing private workspaces are not rebuilt when the template changes.</p>`,
      },
      {
        id: 'register',
        title: 'Use a supported manifest',
        html: `<p>Start with ${source('examples/templates/native-dev.json', 'native-dev.json')} or ${source('examples/templates/native-desktop.json', 'native-desktop.json')}. The current recipe is <code>native-clients-v1</code> on Ubuntu 24.04. It can prepare official clients and browser/desktop tools.</p><p>Register through the Templates dashboard, MCP <code>template_register</code>, or <code>POST /v1/templates</code> with a <code>manifest</code> object or an HTTPS <code>url</code>. Use <code>template_sync</code> to retry fleet synchronization. Inspect per-node status before allocating.</p><p>Manifests reject unknown fields and arbitrary installation scripts. Provider credentials and repository secrets never belong in a template. Increment the manifest revision to publish a changed recipe. A project's future dev start command belongs to the planned environment service, not this manifest.</p>`,
      },
      {
        id: 'allocate',
        title: 'Choose resources at allocation',
        html: `<p>Minimum and Recommended come from the registered template. Custom allocations specify CPU, memory and disk, all at or above its minimum. The controller checks real host capacity again when the job executes.</p><p>For <code>POST /v1/hosts/{host_id}/allocations</code>, a typical body is:</p>` + code(`{
  "name": "ebox-work",
  "template": "native-dev",
  "size": "recommended"
}`) + `<p>Only a successfully installed revision is allocatable on that host. A queued request does not reserve capacity. Keep host headroom for CI and ordinary processes outside Incus.</p>`,
      },
      {
        id: 'warm-pools',
        title: 'Fresh means never assigned',
        html: `<p>A warm pool prepares and pauses credential-free VMs in advance. Allocation claims one permanently, delegates authorized access and verifies readiness. If no compatible reserve exists, the controller can create a cold clone.</p><p>Assigned VMs never return to the fresh pool. Use the actual instance name returned by allocation for later lifecycle calls; a reserve can have a different name from the requested workspace label.</p><p>Paused reserves retain memory allocations. Size the pool against the resources you can keep committed. See ${source('docs/WARM-POOLS.md', 'warm-pool configuration')} and ${source('docs/TEMPLATES.md', 'template synchronization')}.</p>`,
      },
    ],
  },
  {
    slug: 'mcp-api',
    title: 'MCP and HTTP API',
    group: 'Operate',
    summary: 'Manage environments from your terminal or native client through the same authorized controller.',
    sections: [
      {
        id: 'connect',
        title: 'Connect an operator MCP client',
        html: `<p>Register <code>enginebox-fleet-mcp</code> as a stdio server in your client's normal MCP configuration. With a source build:</p>` + code(`export ENGINEBOX_API_URL=http://127.0.0.1:8765
export ENGINEBOX_API_TOKEN_FILE="$HOME/.local/state/enginebox/api.token"
target/release/enginebox-fleet-mcp`) + `<p>The operator token stays on the operator/controller machine. Do not put it inside agent guests or browser storage. Guest browser/computer MCP is a separate, narrower interface. Remote API origins require HTTPS; a local SSH tunnel can keep the configured origin on loopback.</p>`,
      },
      {
        id: 'tools',
        title: 'Use the tool for the operation',
        html: `<table><thead><tr><th>Work</th><th>MCP tools</th></tr></thead><tbody><tr><td>Inspect and allocate</td><td><code>fleet_inventory</code>, <code>fleet_allocate</code>, <code>fleet_create</code></td></tr><tr><td>Lifecycle and jobs</td><td><code>fleet_action</code>, <code>fleet_job</code></td></tr><tr><td>Templates</td><td><code>template_catalog</code>, <code>template_register</code>, <code>template_install</code>, <code>template_sync</code></td></tr><tr><td>Native agents</td><td><code>agent_connections</code>, <code>agent_launch</code>, <code>agent_run</code></td></tr><tr><td>Checkout storage</td><td><code>storage_catalog</code>, <code>storage_inspect</code>, <code>storage_preview</code>, <code>storage_import</code>, <code>storage_fork</code>, <code>storage_action</code></td></tr></tbody></table><p>Tool schemas describe required arguments. Host and project IDs come from the administrator's registry. Native agent launches use assigned running workspaces and return private run results.</p>`,
      },
      {
        id: 'http',
        title: 'The same operations over HTTP',
        html: `<p>All <code>/v1/</code> endpoints require a bearer token. Common entry points are:</p><table><thead><tr><th>Request</th><th>Purpose</th></tr></thead><tbody><tr><td><code>GET /v1/fleet</code></td><td>Hosts, capacity and workspaces</td></tr><tr><td><code>GET /v1/templates</code></td><td>Template manifests and node installation state</td></tr><tr><td><code>GET /v1/agents</code></td><td>Owner account connections</td></tr><tr><td><code>GET /v1/storage</code></td><td>Registered storage nodes and projects</td></tr><tr><td><code>POST /v1/hosts/{host_id}/allocations</code></td><td>Claim a fresh reserve or create a clone</td></tr><tr><td><code>POST /v1/hosts/{host_id}/instances/{name}/actions</code></td><td>Pause, resume, hibernate or stop</td></tr><tr><td><code>GET /v1/jobs/{job_id}</code></td><td>Poll an operation</td></tr></tbody></table><p>Use ${source('docs/FLEET.md', 'the fleet reference')} and ${source('docs/LAYERFS-BACKEND.md', 'storage reference')} for complete request shapes and restrictions.</p>`,
      },
      {
        id: 'jobs',
        title: 'Accepted is not ready',
        html: `<p>Lifecycle, creation and storage mutations return a job. Poll it until completion; a queue receipt is not a ready workspace. Admission, ownership checks and per-host coordination apply equally to the dashboard, API and MCP.</p><p>After an interrupted operation, inspect actual resources before retrying. The controller does not replay an uncertain mutation automatically. Native runs have their own bounded queue and private result endpoint.</p><p>These are infrastructure and launch interfaces. Multi-agent planning, task delegation policies and application data publication remain separate layers.</p>`,
      },
    ],
  },
  {
    slug: 'updates',
    title: 'Installation and updates',
    group: 'Operate',
    summary: 'Stage an exact release, hand off compatible services and retain a tested rollback path.',
    sections: [
      {
        id: 'roles',
        title: 'Install for the machine’s role',
        html: `<p>The terminal installer supports <code>operator</code>, <code>controller</code> and Linux <code>incus-host</code> roles. Run its plan first; dependency installation and provisioning are explicit apply actions. Select an exact published version and matching OS/architecture archive.</p><p>Releases are immutable. Configuration, API tokens, agent homes and private state stay outside release directories. Linux node helpers live under a root-owned prefix. The installer does not silently initialize disks or change unrelated host services.</p><p>See ${source('docs/INSTALL.md', 'installation commands and service setup')}. Use the <a href="/docs/quickstart/">source quickstart</a> when qualifying an unreleased revision.</p>`,
      },
      {
        id: 'handoff',
        title: 'Compatible updates keep a stable front door',
        html: `<ol><li>Stage and validate the exact archive and manifest.</li><li>Start the candidate controller in standby behind the existing supervisor.</li><li>Gate new mutations, drain admitted work and transfer exclusive state ownership.</li><li>Switch routing after readiness succeeds; retain the previous compatible release.</li></ol><p>The supervisor retains the API/dashboard listeners during the handoff. Native agents already running in independent VMs do not need a VM restart for a controller update. The supervisor itself, host kernel, Incus and drivers have separate maintenance lifecycles.</p>`,
      },
      {
        id: 'rollback',
        title: 'Rollback depends on state compatibility',
        html: `<p>Use <code>enginebox-install</code> to stage, update or roll back a release; the ${source('docs/UPDATES.md', 'update guide')} gives the full version, artifact and token-file arguments. Do not replace a live binary or point a service at an arbitrary older directory.</p><p>Rolling back a controller does not restore databases, private homes or VM memory. Older storage binaries reject newer LayerFS workspace records, but older controllers may not understand attachment markers. Detach all LayerFS checkouts before a controller rollback outside the supported compatibility boundary.</p><p>The first Python-to-Rust deployment needs a brief control-plane cutover because the legacy controller lacks the drain and ownership protocol. Preserve its private state; subsequent compatible Rust releases use the supervised handoff.</p>`,
      },
      {
        id: 'qualification',
        title: 'Build checks and deployment are separate',
        html: `<p>The Rust port is merged. Native build/test checks cover Linux, macOS and Windows; isolated macOS/Linux archive staging and Rust-to-Rust update/rollback have been exercised. Existing hosts are not upgraded merely because a PR merged.</p><p>The reference fleet now runs Rust 0.3.1-live.0 with account state preserved, native helper/browser migration on two private workspaces, and native-dev revision 4 plus two fresh reserves synchronized. Fresh physical-host bootstrap, Windows service installation and independently reproduced installs remain release acceptance gates. Check <a href="/docs/platform-support/">current qualification</a> before choosing a deployment target.</p>`,
      },
    ],
  },
  {
    slug: 'platform-support',
    title: 'Availability and platform support',
    group: 'Operate',
    summary: 'What is implemented, what has been exercised and which capabilities are still planned.',
    sections: [
      {
        id: 'platforms',
        title: 'Choose a qualified compute backend',
        html: `<table><thead><tr><th>Platform</th><th>Current scope</th></tr></thead><tbody><tr><td>Linux with Incus/KVM</td><td>VM compute, native helper, templates, warm reserves and workspace lifecycle. Live qualification currently uses x86_64.</td></tr><tr><td>macOS</td><td>Operator/controller target; archive staging and supervised service updates exercised. Manage separate Linux compute nodes.</td></tr><tr><td>Windows</td><td>Native operator/controller build target. Windows service installation and managed provider sign-in require further execution qualification.</td></tr><tr><td>Docker laptop contribution</td><td>Planned. Installing an operator does not add Docker Desktop or personal-machine capacity to the fleet.</td></tr><tr><td>Windows/macOS native compute and GPUs</td><td>Planned backends. GPU detection is not qualification for GPU workloads or saved GPU state.</td></tr></tbody></table><p>Keep explicit CPU, memory and disk limits. Frozen VMs retain memory commitments; stopped VMs still consume disk. Fleet capacity does not account for every unrelated host process.</p>`,
      },
      {
        id: 'capabilities',
        title: 'Feature availability',
        html: `<table><thead><tr><th>Capability</th><th>Status</th></tr></thead><tbody><tr><td>Official Codex, Claude Code, Cursor Agent</td><td>Native integration and account delegation implemented. Rust Codex/Claude coding, exact resume and browser MCP use passed; Cursor remains provider-quota blocked.</td></tr><tr><td>Private native homes</td><td>Persist in each workspace. Cross-workspace memory synchronization and movable tenant volumes are planned.</td></tr><tr><td>Templates and warm pools</td><td>Controller registry, node synchronization and never-reused fresh reserves implemented.</td></tr><tr><td>LayerFS checkout branches</td><td>Controller/API/MCP/dashboard implemented on explicitly provisioned, qualified Linux nodes. General installation does not provision it automatically.</td></tr><tr><td>LayerFS Incus attachment</td><td>Explicit administrator operation; qualified guest file operations and clean lifecycle. No automatic allocation attachment or coordinated RAM hibernation.</td></tr><tr><td>Persistent project core and data forks</td><td>Managed main tracking, optional dev command, sleep/wake and online publication planned. A local stopped-state Wrangler fork prototype exists.</td></tr><tr><td>Public multi-tenant service</td><td>Not implemented. Current authorization assumes one trusted controller owner.</td></tr></tbody></table>`,
      },
      {
        id: 'storage-boundary',
        title: 'Storage qualification is specific',
        html: `<p>The LayerFS preview requires a local Linux store on the supported filesystem/kernel topology and a compatible local Docker runtime. The qualified integration uses Docker API 1.54. It is not a cross-host store or a Docker Desktop filesystem qualification.</p><p>Power-loss durability, bounded history retention, large Unity/Unreal builds, native-provider sessions on the mounted checkout and live Wrangler/SQLite consistency still need qualification. Keep authoritative project state and irreplaceable data on separately qualified persistent storage.</p><p>File checkpoint and VM RAM restoration are different operations. Do not combine them without a consistency protocol. See ${source('docs/LAYERFS.md', 'LayerFS scope and remaining gates')}.</p>`,
      },
      {
        id: 'performance',
        title: 'Read timings as measurements of a specific path',
        html: `<table><thead><tr><th>Measured path</th><th>Result</th></tr></thead><tbody><tr><td>Rust resume from paused VM</td><td>0.512 s median, 0.541 s p95; 20 samples</td></tr><tr><td>Rust start from stopped VM</td><td>6.034 s median, 7.096 s p95; 10 samples</td></tr><tr><td>Rust restore from hibernation</td><td>5.846 s median, 8.053 s p95; 5 samples</td></tr></tbody></table><p>These are readiness timings on the recorded Linux host with a 2-vCPU, 2-GiB VM. The stopped path reused a prepared disk and host caches; it was not a new-machine install. The measurements did not use a LayerFS-attached VM. They do not establish a controlled speedup over Python or a universal subsecond boot promise.</p><p>The LayerFS backend created fresh Docker workspaces in 1.437 seconds and a workspace from a prepared checkpoint in 1.549 seconds in its small-fixture qualification. These were individual observations with prepared images and caches. A cheap branch reference is not a booted VM or a completed application start. See ${source('docs/reports/RUST-BOOT-BENCHMARKS.md', 'VM timing evidence')} and ${source('docs/reports/LAYERFS-BACKEND-VALIDATION.md', 'storage measurements')}.</p>`,
      },
      {
        id: 'release-gates',
        title: 'Before a wider release',
        html: `<p>The reference fleet completed its initial Rust cutover on October 20, 2026. Separate disposable tests proved Codex/Claude coding, exact conversation resume and browser MCP use; Cursor remains provider-quota blocked. A fresh revision-4 warm allocation then passed real Codex/Claude code assertions through the installed operator MCP and HTTP controller, followed by exact conversation resume after MCP pause/resume with boot identity preserved. Cursor was not retried. Inference on LayerFS attachments, a fresh physical-host install, Windows service execution and an independent installation reproduction remain acceptance work.</p><p>Cloudflare environment publication is new product work, not a missing checkbox on the Rust migration. Its acceptance test must exercise concurrent writes, interruption during capture, complete binding state, two isolated forks and restart at representative sizes.</p><p>Use ${source('docs/reports/VALIDATION.md', 'the validation record')} for evidence and ${source('docs/IMPLEMENTATION-PLAN.md', 'the implementation plan')} for delivery order.</p>`,
      },
    ],
  },
];
