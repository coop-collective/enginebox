// Test-only API fixture. The application never imports this module.
import { createServer } from "node:http";
const GiB = 1024 ** 3;
const token = "fixture-api-token-server-only-12345";
const minimum = { cpu: 2, memory_gib: 2, disk_gib: 30 };
const recommended = { cpu: 4, memory_gib: 4, disk_gib: 40 };
let contributors = [];
let mode,
  jobs,
  instances,
  templates,
  ciTemplates,
  authSessions,
  connectedAccounts,
  syncProgress,
  retried,
  storageRows,
  storageRequests,
  storageImported,
  githubProjects,
  githubConnections,
  githubReviews;
function manifest(id = "native", revision = 1) {
  return {
    schema_version: 1,
    id,
    revision,
    label: id === "native" ? "Native coding agents" : id,
    description: "Prepared native clients and a browser.",
    recipe: "native-clients-v1",
    source_image: "images:ubuntu/24.04/cloud",
    minimum,
    recommended,
    native_clients: ["codex", "cursor", "claude"],
    config: { browser: true },
    manifest_digest: String(revision).padStart(64, "0"),
    deployments: [],
    sync: {
      desired_revision: revision,
      nodes: [
        { host_id: "metal-01", label: "Build machine", status: "pending" },
        {
          host_id: "metal-02",
          label: "Spare machine",
          status: "pending",
          error:
            "Node is offline; synchronization will retry when it reconnects.",
        },
      ],
    },
  };
}
function reset() {
  mode = "normal";
  contributors = [];
  jobs = new Map();
  authSessions = new Map();
  connectedAccounts = new Set();
  syncProgress = new Map();
  retried = new Set();
  storageRequests = [];
  githubProjects = [];
  githubConnections = new Map();
  githubReviews = new Map();
  storageImported = new Set(["game"]);
  storageRows = [
    { name: "prepared", project: "game", status: "ready", last_checkpoint: { status: "Committed", head: "fixture-head" } },
    { name: "worker-a", project: "game", status: "ready" },
    { name: "worker-b", project: "game", status: "ready" },
    { name: "interrupted", project: "game", status: "exec", last_error: "Interrupted command outcome is unknown." },
  ];
  const native = manifest();
  native.deployments.push({
    host_id: "metal-01",
    source: "ebox-native-base",
    snapshot: "clean",
    revision: 1,
    manifest_digest: native.manifest_digest,
    in_sync: true,
  });
  native.sync.nodes[0].status = "ready";
  templates = [native];
  ciTemplates = [];
  instances = [
    {
      name: "ebox-editor",
      status: "Running",
      stateful: false,
      ipv4: ["10.88.0.14"],
      cpu: 4,
      memory_bytes: 4 * GiB,
      memory_usage_bytes: 1.4 * GiB,
      managed: true,
    },
    {
      name: "ebox-review",
      status: "Frozen",
      stateful: false,
      ipv4: ["10.88.0.15"],
      cpu: 4,
      memory_bytes: 4 * GiB,
      memory_usage_bytes: null,
      managed: true,
    },
    {
      name: "ebox-native-base",
      status: "Stopped",
      stateful: false,
      ipv4: [],
      cpu: 4,
      memory_bytes: 4 * GiB,
      managed: false,
    },
  ];
}
function poolMode() {
  return ["warm", "pool-preparing"].includes(mode);
}
function hasMetadata() {
  return poolMode() || mode === "typed" || mode === "catalog";
}
let environmentRoutes = [];
const environmentCatalog = () => ({
  projects: [{ id: 'p-demo', host: 'metal-01', project: 'demo', status: 'ready', healthy: true, urls: ['https://demo.dev.example.com'] }, { id: 'p-failed', host: 'metal-01', project: 'failed-app', status: 'failed', healthy: false, error: 'setup-failed', urls: [] }],
  baseDomains: ['dev.example.com'], transportEntrypoints: [{ name: 'game-udp', protocol: 'udp', port: 21000, public: false }], routes: environmentRoutes,
  environments: [{ id: 'main', source: { kind: 'git', branch: 'main' }, services: [{ name: 'web', protocol: 'http' }, { name: 'game', protocol: 'udp' }], status: 'ready', healthy: true, revision: 'a'.repeat(40), desiredRevision: 'a'.repeat(40), cpu: 1, memoryMiB: 256 }],
});
reset();
createServer(async (req, res) => {
  function send(status, body) {
    // Controller-owned Compute projections; the UI must not invent these from
    // the legacy inventories. Fixture values deliberately differ from hardware.
    if (status === 200 && req.url === '/v1/fleet') body.compute_nodes = body.hosts.map(h => ({
      id: `host:${h.id}`, source_id: h.id, label: h.label, backend: 'incus', platform: 'linux', registered: true, online: h.online,
      registration: {method: 'controller_config'},
      runtime: {state: !h.online ? 'offline' : poolMode() ? 'capacity_limited' : 'ready', running_workspaces: h.instances.filter(i => i.managed && i.status === 'Running').length, active_job: null, last_seen: h.online ? Math.floor(Date.now()/1000) : null},
      resources: {budget: h.online ? {cpu: 28, memory_bytes: 112*GiB, disk_bytes: 900*GiB} : null, available: h.online ? {cpu: 16, memory_bytes: 64*GiB, disk_bytes: poolMode() ? 0 : 500*GiB} : null},
      templates: h.online ? ['native'] : [], capabilities: {vm_workspaces: true, template_workspaces: false, ci_jobs: false},
      management: {revoke: false, reconcile: false, limits: 'controller_config'}, error: h.error ?? null,
    }));
    if (status === 200 && req.url === '/v1/contributors') body.compute_nodes = body.nodes.filter(n => n.enrolled && !n.revoked).map(n => ({
      id: `worker:${n.id}`, source_id: n.id, label: n.label, backend: n.tools.backend, platform: 'windows', registered: true, online: n.online,
      registration: {method: 'enrollment'}, runtime: {state: n.blocked ? 'needs_attention' : !n.online ? 'offline' : n.tools.workspace ? 'ready' : 'preparing', running_workspaces: null, active_job: null, last_seen: n.online ? Math.floor(Date.now()/1000) : null},
      resources: {budget: {cpu:n.budget.cpu,memory_bytes:n.budget.memory_gib*GiB,disk_bytes:n.budget.disk_gib*GiB}, available:null},
      templates: n.tools.workspace ? ['native-dev'] : [], capabilities: {vm_workspaces:false,template_workspaces:!!n.tools.workspace,ci_jobs:false},
      management: {revoke:true,reconcile:n.online && n.blocked && n.tools.worker_running === false,limits:'local_installer'},error:null,
    }));
    res.writeHead(status, { "Content-Type": "application/json" });
    res.end(JSON.stringify(body));
  }
  let body = "";
  for await (const chunk of req) body += chunk;
  const data = body ? JSON.parse(body) : {};
  if (req.url === "/healthz") return send(200, { ok: true });
  if (req.url === "/__test/reset") {
    reset();
    environmentRoutes = [];
    return send(200, { ok: true });
  }
  if (req.url === "/__test/contributor-online") { contributors = contributors.map(n => ({...n,enrolled:true,online:true,budget:{cpu:2,memory_gib:4,disk_gib:20},tools:{workspace:true,backend:'wsl2-layerfs'}})); return send(200,{ok:true}); }
  if (req.url === '/__test/contributor-inventory') {
    const base={enrolled:true,revoked:false,blocked:false,online:false,budget:{cpu:4,memory_gib:8,disk_gib:50},workloads:{ci:true,agents:true},tools:{workspace:true,backend:'wsl2-layerfs'}};
    contributors=[{...base,id:'1'.repeat(32),label:'Render RTX',online:true},{...base,id:'2'.repeat(32),label:'Build workstation'},{...base,id:'3'.repeat(32),label:'Retired machine',revoked:true},...Array.from({length:12},(_,i)=>({...base,id:'f'+i.toString(16).padStart(31,'0'),label:`Windows setup ${i+1}`,enrolled:false,tools:{}}))];
    return send(200,{ok:true});
  }
  if (req.url === "/__test/storage-requests") return send(200, storageRequests);
  if (req.url === "/__test/mode") {
    mode = data.mode;
    if (mode === "accounts-unsupported") connectedAccounts.add("cursor");
    if (mode === "accounts-reauth") connectedAccounts.add("claude");
    if (mode.startsWith("usage-"))
      for (const provider of ["codex", "claude", "cursor"]) connectedAccounts.add(provider);
    if (mode === "catalog-new-node")
      for (const record of templates) {
        record.sync.nodes.push({
          host_id: "metal-03",
          label: "New machine",
          status: "pending",
        });
      }
    if (poolMode())
      for (const state of ["ready", "preparing", "quarantined"]) {
        if (!instances.some((item) => item.name === `ebox-pool-${state}`))
          instances.push({
            name: `ebox-pool-${state}`,
            status: state === "ready" ? "Frozen" : "Running",
            stateful: false,
            ipv4: [],
            cpu: 4,
            memory_bytes: 4 * GiB,
            managed: true,
            template: "native",
            warm_pool_state:
              mode === "pool-preparing" && state === "ready"
                ? "preparing"
                : state,
          });
      }
    return send(200, { ok: true });
  }
  if (req.headers.authorization !== `Bearer ${token}`)
    return send(401, { error: "Wrong backend token" });
  if (req.url === "/v1/ci/templates") {
    if (req.method === "POST") {
      ciTemplates = ciTemplates.filter((item) => item.manifest.id !== data.manifest.id);
      ciTemplates.push({ manifest: data.manifest, enabled: false, nodes: [{ node_id: "ci-linux", status: "disabled", verified_at: null, error: null }] });
    }
    for (const recipe of ciTemplates) for (const node of recipe.nodes) {
      if (node.status === "installing" && Date.now() > node.finish) {
        node.status = node.retried ? "ready" : "failed";
        node.error = node.retried ? null : "verification-failed";
        node.verified_at = node.retried ? Math.floor(Date.now() / 1000) : null;
      }
    }
    return send(200, { schema_version: 1, templates: ciTemplates });
  }
  const ciAction = req.url.match(/^\/v1\/ci\/templates\/([^/]+)\/(enable|disable|retry)$/);
  if (ciAction && req.method === "POST") {
    const recipe = ciTemplates.find((item) => item.manifest.id === ciAction[1]);
    if (!recipe) return send(400, { error: "Unknown CI template" });
    if (ciAction[2] !== "retry") recipe.enabled = ciAction[2] === "enable";
    for (const node of recipe.nodes) {
      node.status = recipe.enabled ? "installing" : "disabled";
      node.error = null; node.verified_at = null; node.finish = Date.now() + 250;
      if (ciAction[2] === "retry") node.retried = true;
    }
    return send(200, { schema_version: 1, templates: ciTemplates });
  }
  if (req.url === '/v1/environments' && req.method === 'GET') return send(200, environmentCatalog());
  if (req.url === '/__enginebox/status') return send(200,{node_protocol:1});
  if (req.url === '/v1/contributors' && req.method === 'GET') return send(200,{nodes:contributors});
  if (req.url === '/v1/contributors/enrollments' && req.method === 'POST') {
    const id='a'.repeat(32); contributors.push({...data,id,enrolled:false,revoked:false,blocked:false,online:false,tools:{}});
    return send(200,{id,code:`${id}.${'b'.repeat(64)}`,expires:Math.floor(Date.now()/1000)+600});
  }
  const renewContributor = req.url.match(/^\/v1\/contributors\/([a-f0-9]{32})\/renew-enrollment$/);
  if (renewContributor && req.method === 'POST') {
    const node=contributors.find(n=>n.id===renewContributor[1]);
    if (!node || node.enrolled || node.revoked) return send(400,{error:'Only a pending registration can be renewed.'});
    return send(200,{id:node.id,code:`${node.id}.${'c'.repeat(64)}`,expires:Math.floor(Date.now()/1000)+3600});
  }
  const revokeContributor=req.url.match(/^\/v1\/contributors\/([a-f0-9]{32})\/revoke$/);
  if (revokeContributor) { contributors=contributors.map(n=>n.id===revokeContributor[1]?{...n,revoked:true}:n);return send(200,{revoked:true}); }
  if (req.url === '/v1/environments/routes' && req.method === 'POST') {
    environmentRoutes = environmentRoutes.filter(r => r.id !== (data.remove ?? data.id));
    if (!data.remove) environmentRoutes.push(data);
    return send(200, environmentCatalog());
  }
  if (req.url === "/v1/workspaces" && req.method === "GET") {
    const projects = [...new Set(["game", "demo", ...githubProjects.map(project => project.id)])];
    return send(200, { workspaces: projects.map(id => ({ id, repositories: [], pools: [], runs: [], storage_nodes: ["metal-01"], storage_usage: [], checkouts: [], operations: [], launch_events: [], source_observations: [], integration_observations: [] })), coverage: { run_history_limit: 256, operation_history_limit: 200, operations_per_workspace_limit: 30 } });
  }
  if (req.url === "/v1/repository-backends" && req.method === "GET") return send(200, { backend: { configured: false } });
  if (req.url === "/v1/github" && req.method === "GET") return send(200, { configured: false, repositories: [] });
  if (req.url === "/v1/storage" && req.method === "GET") {
    if (mode === "storage-error") return send(503, { error: "Storage catalog unavailable." });
    return send(200, { backend: "layerfs", configured: mode !== "storage-empty", nodes: mode === "storage-empty" ? [] : [{ host_id: "metal-01", status: "registered", github: { enabled: true, private_auth_configured: false }, projects: [{ id: "game" }, { id: "demo" }] }], incus_attachment: false, automatic_replay: false });
  }
  const storage = req.url.match(/^\/v1\/storage\/hosts\/metal-01\/(inspect|list|preview|import|fork|deps|checkpoint|remount|close|recover|github-preview|github-import|github-settings|github-fork|github-promote|github-delivery|dev-start|dev-status)$/);
  if (storage && req.method === "POST") {
    storageRequests.push({ operation: storage[1], body: data });
    const id = `storage-${jobs.size + 1}`;
    const job = { id, status: "queued", polls: 0, storage: storage[1], data };
    jobs.set(id, job);
    return send(202, { job: { id, status: "queued" } });
  }
  const storageJob = req.method === "GET" && req.url.startsWith("/v1/jobs/") && jobs.get(req.url.split("/").at(-1));
  if (storageJob?.storage) {
    const job = storageJob;
    job.polls++;
    if (job.status === "queued") job.status = "running";
    if (job.status === "running" && job.polls >= (mode === "storage-slow" && !["list", "preview", "inspect"].includes(job.storage) ? 4 : 2)) {
      const operation = job.storage;
      const input = job.data;
      const row = storageRows.find((row) => row.name === input.name);
      let result = { ok: true };
      job.status = "succeeded";
      if (operation === "list") result = { workspaces: structuredClone(storageRows), github_projects: structuredClone(githubProjects) };
      else if (operation === "github-preview") {
        if (mode === "github-fetch-failed") { job.status = "failed"; job.error = "GitHub transfer failed; check repository access."; }
        else { githubReviews.set(input.project, input); result = { commit: "c".repeat(40), approval: "d".repeat(64), files: ["README.md", "src/main.ts"], bytes: 2000 }; }
      } else if (operation === "github-import") {
        if (input.revision !== "c".repeat(40) || input.approval !== "d".repeat(64) || input.credential_free !== true || !githubReviews.has(input.project)) { job.status = "failed"; job.error = "Exact source review required."; }
        else {
          result.dev = { status: "queued" };
          const reviewed = githubReviews.get(input.project);
          githubProjects.push({ id: input.project, repository: reviewed.repository, branch: reviewed.branch || "main", current: { project: "gh-fixture", commit: input.revision } });
          githubConnections.set(input.project, { main: { revision: "e".repeat(64), sequence: 0 }, sync: { enabled: false, active: null } });
        }
      } else if (operation === "dev-start" || operation === "dev-status") {
        result = { host: 'metal-01', project: input.project, status: operation === 'dev-start' ? 'queued' : 'ready', urls: operation === 'dev-status' ? ['https://demo.dev.example.com'] : [] };
      } else if (operation === "github-settings") {
        const connection = githubConnections.get(input.project);
        if (!connection) { job.status = "failed"; job.error = "Unknown GitHub project."; }
        else { if (typeof input.enabled === "boolean") connection.sync.enabled = input.enabled; result = structuredClone(connection); }
      } else if (operation === "github-fork") {
        storageRows.push({ name: input.name, project: "gh-fixture", github_project: input.project, status: "ready" });
      } else if (operation === "github-promote") {
        const connection = githubConnections.get(input.project);
        if (!connection || input.expected_main !== connection.main.revision || row?.status !== "closed" || !row.last_checkpoint) { job.status = "failed"; job.error = "Main changed or the branch is not closed and checkpointed."; }
        else {
          connection.main = { revision: "f".repeat(64), sequence: 1 };
          if (connection.sync.enabled) connection.sync.active = { id: "sync-fixture", status: "needs_agent", conflicts: ["src/main.ts"] };
          result = { publication: structuredClone(connection.main), sync: structuredClone(connection.sync) };
        }
      } else if (operation === "github-delivery") {
        const connection = githubConnections.get(input.project);
        if (input.action === "agent-task") result = { kind: "github-reconciliation", attempt: input.attempt, environment: { kind: "docker", container: "ebox-sync-fixture", workdir: "/workspace", push_credentials: false } };
        else if (input.action === "resolve") { connection.sync.active.status = "delivered"; connection.sync.active.conflicts = []; }
        else if (input.action === "retry") connection.sync.active.status = "needs_agent";
      }
      else if (operation === "preview") result = { commit: "a".repeat(40), approval: "b".repeat(64), files: ["package.json", "src/main.ts"], bytes: 5242 };
      else if (operation === "import") {
        if (input.revision !== "a".repeat(40) || input.approval !== "b".repeat(64) || input.credential_free !== true || storageImported.has(input.project)) {
          job.status = "failed"; job.error = "Source approval changed or project already imported.";
        } else storageImported.add(input.project);
      } else if (operation === "fork") {
        if (!storageImported.has(input.project) || row) { job.status = "failed"; job.error = "Import the project first and choose a new checkout name."; }
        else { storageRows.push({ name: input.name, project: input.project, status: "ready" }); result = { ok: true, workspace: input.name, path: "/project" }; }
      } else if (operation !== "inspect") {
        if (!row) { job.status = "failed"; job.error = "Unknown checkout."; }
        else if (mode === "storage-failed" || (operation === "close" && row.name === "worker-b")) { job.status = "failed"; job.error = "Workspace has uncheckpointed changes; run checkpoint before close."; }
        else if (operation === "close") row.status = "closed";
        else if (operation === "checkpoint") row.last_checkpoint = { status: "Committed", head: "fixture-checkpoint" };
        else if (operation === "recover") { row.status = "ready"; row.last_error = null; }
      }
      if (job.status === "succeeded") job.result = { backend: "layerfs", host_id: "metal-01", operation: `storage_${operation}`, result };
    }
    return send(200, { job: { id: job.id, status: job.status, error: job.error, result: job.result } });
  }
  if (req.url === "/v1/templates") {
    if (req.method === "GET") {
      for (const record of templates)
        for (const node of record.sync.nodes) {
          if (
            node.host_id === "metal-02" ||
            !["pending", "syncing"].includes(node.status)
          )
            continue;
          const key = `${record.id}/${record.revision}/${node.host_id}`;
          const count = (syncProgress.get(key) ?? 0) + 1;
          syncProgress.set(key, count);
          node.status =
            count < 3
              ? "syncing"
              : mode === "catalog-sync-error" && !retried.has(record.id)
                ? "failed"
                : "ready";
          if (node.status === "failed")
            node.error = "Not enough disk capacity to prepare this template.";
          if (node.status === "ready") {
            delete node.error;
            record.deployments = record.deployments.filter(
              (item) => item.host_id !== node.host_id,
            );
            record.deployments.push({
              host_id: node.host_id,
              source: `ebox-template-${record.id}`,
              snapshot: "clean",
              revision: record.revision,
              manifest_digest: record.manifest_digest,
              in_sync: true,
            });
          }
        }
      return send(200, { templates });
    }
    if (req.method === "POST") {
      const value = data.manifest ?? manifest("url-native");
      if (!value.id || !value.minimum || !value.recommended)
        return send(400, {
          error: "Template resource requirements are required.",
        });
      const existing = templates.find((item) => item.id === value.id);
      const record = {
        ...value,
        manifest_digest: String(value.revision).padStart(64, "0"),
        deployments: existing?.deployments ?? [],
        sync: {
          desired_revision: value.revision,
          nodes: (existing?.sync.nodes ?? manifest().sync.nodes).map(
            (node) => ({
              host_id: node.host_id,
              label: node.label,
              status:
                existing?.revision === value.revision ? node.status : "pending",
              ...(node.host_id === "metal-02"
                ? {
                    error:
                      "Node is offline; synchronization will retry when it reconnects.",
                  }
                : {}),
            }),
          ),
        },
      };
      record.deployments = record.deployments.map((item) => ({
        ...item,
        in_sync: item.manifest_digest === record.manifest_digest,
      }));
      templates = [
        ...templates.filter((item) => item.id !== record.id),
        record,
      ];
      return send(200, { template: record });
    }
  }
  const sync = req.url.match(/^\/v1\/templates\/([a-z][a-z0-9-]*)\/sync$/);
  if (req.method === "POST" && sync) {
    const record = templates.find((item) => item.id === sync[1]);
    if (!record) return send(404, { error: "Unknown template." });
    retried.add(record.id);
    for (const node of record.sync.nodes)
      if (node.status !== "ready") {
        node.status = "pending";
        delete node.error;
      }
    return send(202, { template: record.id, requested: true });
  }
  if (req.url === "/v1/fleet") {
    if (mode === "error")
      return send(503, { error: "Fixture host inventory is unavailable." });
    return send(200, {
      hosts:
        mode === "empty"
          ? []
          : [
              {
                id: "metal-01",
                label: "Build machine",
                online: true,
                capacity: {
                  cpu_total: 32,
                  memory_total_bytes: 128 * GiB,
                  memory_available_bytes: 104 * GiB,
                  disk_free_bytes: 850 * GiB,
                  reserve_cpu: 4,
                  reserve_memory_bytes: 16 * GiB,
                  allocated_cpu: instances.length * 4,
                  allocated_memory_bytes: instances.length * 4 * GiB,
                  cpu_available: 16,
                  memory_budget_available_bytes: 64 * GiB,
                  disk_budget_available_bytes: 500 * GiB,
                  instance_slots_available: poolMode() ? 0 : 8,
                  max_additional: poolMode()
                    ? { small: 0, standard: 0, large: 0 }
                    : { small: 8, standard: 4, large: 2 },
                },
                instances,
                ...(poolMode()
                  ? {
                      warm_pools: [
                        {
                          template: "native",
                          size: "standard",
                          resources: recommended,
                          target: 2,
                          ready: instances.filter(
                            (item) => item.warm_pool_state === "ready",
                          ).length,
                          preparing: instances.filter(
                            (item) => item.warm_pool_state === "preparing",
                          ).length,
                          ...(mode === "pool-preparing"
                            ? { error: "Refill is waiting for host capacity." }
                            : {}),
                        },
                      ],
                    }
                  : {}),
              },
              {
                id: "metal-02",
                label: "Spare machine",
                online: false,
                error: "SSH connection timed out.",
                instances: [],
              },
            ],
      templates: templates
        .filter((item) =>
          item.deployments.some((deployment) => deployment.in_sync),
        )
        .map((item) => ({
          id: item.id,
          label: item.label,
          ...(hasMetadata()
            ? {
                minimum: item.minimum,
                recommended: item.recommended,
                host_resources: {
                  "metal-01":
                    mode === "typed"
                      ? {
                          minimum: { cpu: 2, memory_gib: 3, disk_gib: 35 },
                          recommended: { cpu: 4, memory_gib: 6, disk_gib: 50 },
                        }
                      : {
                          minimum: item.minimum,
                          recommended: item.recommended,
                        },
                },
                host_ids: item.deployments
                  .filter((deployment) => deployment.in_sync)
                  .map((deployment) => deployment.host_id),
              }
            : {}),
        })),
      sizes: {
        small: { cpu: 2, memory_gib: 2 },
        standard: { cpu: 4, memory_gib: 4 },
        large: { cpu: 8, memory_gib: 8 },
      },
    });
  }
  const usage = req.url.match(/^\/v1\/agents\/(codex|claude|cursor)\/usage$/);
  if (usage && req.method === "GET") {
    const provider = usage[1];
    if (mode === "usage-error") return send(503, { error: "Usage unavailable" });
    const available = provider === "codex" && connectedAccounts.has(provider);
    const stale = mode === "usage-stale";
    return send(200, { usage: {
      provider, scope: "controller_account", status: available ? "available" : "unavailable",
      detail: provider === "claude" ? "Claude does not provide an on-demand allowance through this integration. Check provider usage."
        : "Cursor does not expose personal allowance through the supported CLI. Check provider usage.",
      source: available ? "Codex native app-server" : null,
      observed_at: available ? new Date().toISOString() : null,
      freshness: available ? "current" : "unknown", plan: available ? "pro" : null,
      billing_cycle: null, refresh_after_seconds: 60,
      allowance_windows: available ? [{
        id: "codex:primary", label: "codex", used_percentage: 25,
        remaining_percentage: stale ? null : 75, window_minutes: 10080,
        resets_at: new Date(Date.now() + (stale ? -1000 : 86400_000)).toISOString(),
        freshness: stale ? "stale" : "current",
      }] : [],
      credits: available ? [{ label: "codex", balance: "12.50", unlimited: false, unit: "provider credits" }] : [],
    } });
  }
  const accounts = req.url.match(
    /^\/v1\/agents(?:\/(codex|claude|cursor)\/(login|sessions)(?:\/([a-f0-9]{32})(?:\/(input|cancel))?)?)?$/,
  );
  if (accounts) {
    const [, provider, operation, id, inputAction] = accounts;
    const publicSession = (session) => {
      const { polls, ...value } = session;
      return value;
    };
    if (req.method === "GET" && !provider) {
      if (mode === "accounts-error")
        return send(503, { error: "Native account status is unavailable." });
      if (mode === "accounts-delayed")
        await new Promise((resolve) => setTimeout(resolve, 800));
      return send(200, {
        providers: ["codex", "claude", "cursor"].map((provider) => {
          const installed = !(
            mode === "accounts-unavailable" && provider === "cursor"
          );
          const connected = connectedAccounts.has(provider);
          const session = [...authSessions.values()]
            .reverse()
            .find(
              (item) =>
                item.provider === provider &&
                ["starting", "waiting"].includes(item.status),
            );
          return {
            id: provider,
            label: provider,
            installed,
            status: installed
              ? connected
                ? "connected"
                : "disconnected"
              : "unavailable",
            auth_method: connected ? "account" : "none",
            detail: connected
              ? "Account connected on the controller."
              : installed
                ? "No controller account connected."
                : "Native client is not installed on the controller.",
            delegation_status:
              mode === "accounts-unsupported" && provider === "cursor"
                ? "unsupported"
                : mode === "accounts-reauth" && provider === "claude"
                  ? "reauth_required"
                  : connected
                    ? "ready"
                    : "unknown",
            ...(connected ? { workspace_count: 2 } : {}),
            ...(session ? { session: publicSession(session) } : {}),
          };
        }),
      });
    }
    if (req.method === "POST" && operation === "login") {
      const id = String(authSessions.size + 1).padStart(32, "0");
      const session = {
        id,
        provider,
        status: "waiting",
        input_required: provider === "claude",
        polls: 0,
        authorization_url: {
          codex: "https://auth.openai.com/codex/device",
          claude:
            "https://claude.com/cai/oauth/authorize?code=true&client_id=fixture",
          cursor: "https://cursor.com/loginDeepControl?challenge=fixture",
        }[provider],
        ...(provider === "codex" ? { user_code: "ABCD-EFGH" } : {}),
        message:
          provider === "claude"
            ? "Complete sign-in, then paste the authorization code."
            : "Finish signing in with your provider in the browser.",
        expires_at: new Date(Date.now() + 15 * 60_000).toISOString(),
      };
      authSessions.set(id, session);
      return send(200, { session: publicSession(session) });
    }
    const session = authSessions.get(id);
    if (!session || session.provider !== provider)
      return send(404, { error: "Unknown sign-in session." });
    if (req.method === "POST" && inputAction === "cancel") {
      session.status = "cancelled";
      session.message = "Sign-in cancelled.";
      session.input_required = false;
      return send(200, { session: publicSession(session) });
    }
    if (req.method === "POST" && inputAction === "input") {
      if (
        provider !== "claude" ||
        !session.input_required ||
        data.text !== "fixture-owner-code"
      )
        return send(400, { error: "Authorization code was not accepted." });
      session.input_required = false;
      session.message = "Completing native sign-in.";
      return send(200, { session: publicSession(session) });
    }
    if (req.method === "GET") {
      if (mode === "accounts-delayed")
        await new Promise((resolve) => setTimeout(resolve, 800));
      if (["starting", "waiting"].includes(session.status)) {
        session.polls++;
        if (
          !["accounts-pending", "accounts-delayed"].includes(mode) &&
          !session.input_required &&
          session.polls >= 2
        ) {
          session.status = "succeeded";
          session.message = "Native account sign-in completed.";
          connectedAccounts.add(provider);
        }
      }
      return send(200, { session: publicSession(session) });
    }
  }
  const action = req.url.match(
    /^\/v1\/hosts\/([^/]+)\/instances\/([^/]+)\/actions$/,
  );
  const install = req.url.match(
    /^\/v1\/hosts\/([^/]+)\/templates\/([^/]+)\/install$/,
  );
  const allocation = req.url === "/v1/hosts/metal-01/allocations";
  if (
    req.method === "POST" &&
    (req.url === "/v1/hosts/metal-01/instances" ||
      allocation ||
      action ||
      install)
  ) {
    if (mode === "slow-actions" && action)
      await new Promise((resolve) => setTimeout(resolve, 1200));
    const candidate =
      allocation && instances.find((item) => item.warm_pool_state === "ready");
    const id = `job-${jobs.size + 1}`;
    const job = {
      id,
      status: "queued",
      polls: 0,
      name: action?.[2] ?? data.name,
      template: install?.[2],
      action: install
        ? "install"
        : (data.action ?? (allocation ? "allocate" : "create")),
      data,
      candidate: candidate?.name,
    };
    jobs.set(id, job);
    return send(202, { job: { id, status: "queued" } });
  }
  const job = jobs.get(req.url.split("/").at(-1));
  if (req.method === "GET" && req.url.startsWith("/v1/jobs/") && job) {
    job.polls++;
    if (job.polls === 1) job.status = "running";
    if (job.polls === 2) {
      if (job.name === "ebox-fail") {
        job.status = "failed";
        job.error = "The selected host does not have enough free storage.";
      } else {
        job.status = "succeeded";
        if (job.action === "install") {
          const record = templates.find((item) => item.id === job.template);
          record.deployments = [
            {
              host_id: "metal-01",
              source: `ebox-template-${record.id}`,
              snapshot: "clean",
              revision: record.revision,
              manifest_digest: record.manifest_digest,
              in_sync: true,
            },
          ];
        } else if (job.action === "create" || job.action === "allocate") {
          let vm = instances.find((item) => item.name === job.candidate);
          if (vm) {
            vm.warm_pool_state = "claimed";
            vm.workspace_name = job.name;
            vm.status = "Running";
          } else {
            vm = {
              name: job.name,
              status: job.action === "allocate" ? "Running" : "Stopped",
              stateful: false,
              ipv4: [],
              cpu: job.data.resources?.cpu ?? 2,
              memory_bytes: (job.data.resources?.memory_gib ?? 2) * GiB,
              managed: true,
            };
            instances.push(vm);
          }
          if (job.action === "allocate")
            job.result = {
              name: vm.name,
              workspace_name: job.name,
              startup: job.candidate ? "warm" : "cold",
              ready_seconds: job.candidate ? 0.15 : 12.4,
            };
        } else {
          const vm = instances.find((item) => item.name === job.name);
          vm.status = {
            pause: "Frozen",
            resume: "Running",
            hibernate: "Stopped",
            stop: "Stopped",
          }[job.action];
          vm.stateful = job.action === "hibernate";
        }
      }
    }
    return send(200, {
      job: {
        id: job.id,
        status: job.status,
        error: job.error,
        result: job.result,
      },
    });
  }
  send(404, { error: "Not found" });
}).listen(Number(process.env.ENGINEBOX_TEST_FLEET_PORT ?? process.env.EBOX_TEST_FLEET_PORT ?? "18765"), "127.0.0.1", () =>
  process.stdout.write("Test fleet API listening on loopback.\n"),
);
