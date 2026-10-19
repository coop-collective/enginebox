// Test-only API fixture. The application never imports this module.
import { createServer } from "node:http";
const GiB = 1024 ** 3;
const token = "fixture-api-token-server-only-12345";
const minimum = { cpu: 2, memory_gib: 2, disk_gib: 30 };
const recommended = { cpu: 4, memory_gib: 4, disk_gib: 40 };
let mode, jobs, instances, templates, authSessions, connectedAccounts;
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
  };
}
function reset() {
  mode = "normal";
  jobs = new Map();
  authSessions = new Map();
  connectedAccounts = new Set();
  const native = manifest();
  native.deployments.push({
    host_id: "metal-01",
    source: "ebox-native-base",
    snapshot: "clean",
    revision: 1,
    manifest_digest: native.manifest_digest,
    in_sync: true,
  });
  templates = [native];
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
reset();
createServer(async (req, res) => {
  function send(status, body) {
    res.writeHead(status, { "Content-Type": "application/json" });
    res.end(JSON.stringify(body));
  }
  let body = "";
  for await (const chunk of req) body += chunk;
  const data = body ? JSON.parse(body) : {};
  if (req.url === "/healthz") return send(200, { ok: true });
  if (req.url === "/__test/reset") {
    reset();
    return send(200, { ok: true });
  }
  if (req.url === "/__test/mode") {
    mode = data.mode;
    if (mode.startsWith("accounts")) {
      instances.find((item) => item.name === "ebox-review").status = "Running";
      connectedAccounts.add("ebox-review/codex");
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
  if (req.url === "/v1/templates") {
    if (req.method === "GET") return send(200, { templates });
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
        .filter((item) => item.deployments.length)
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
                host_ids: item.deployments.map(
                  (deployment) => deployment.host_id,
                ),
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
  const accounts = req.url.match(
    /^\/v1\/hosts\/metal-01\/instances\/(ebox-[a-z0-9-]+)\/accounts(?:\/(codex|claude|cursor)\/(login|sessions)(?:\/([a-f0-9]{32})(?:\/(input|cancel))?)?)?$/,
  );
  if (accounts) {
    const [, name, provider, operation, id, inputAction] = accounts;
    const vm = instances.find((item) => item.name === name);
    if (
      !vm?.managed ||
      (vm.warm_pool_state && vm.warm_pool_state !== "claimed")
    )
      return send(400, { error: "Choose an assigned workspace." });
    if (vm.status !== "Running")
      return send(409, { error: "Resume this workspace to connect accounts." });
    const publicSession = (session) => {
      const { polls, ...value } = session;
      return value;
    };
    if (req.method === "GET" && !provider) {
      if (mode === "accounts-error")
        return send(503, { error: "Native account status is unavailable." });
      if (mode === "accounts-delayed" && name === "ebox-editor")
        await new Promise((resolve) => setTimeout(resolve, 800));
      return send(200, {
        providers: ["codex", "claude", "cursor"].map((provider) => {
          const installed = !(
            mode === "accounts-unavailable" && provider === "cursor"
          );
          const connected = connectedAccounts.has(`${name}/${provider}`);
          const session = [...authSessions.values()]
            .reverse()
            .find(
              (item) =>
                item.name === name &&
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
              ? `Account connected in ${name}.`
              : "No account connected in this workspace.",
            ...(session ? { session: publicSession(session) } : {}),
          };
        }),
      });
    }
    if (req.method === "POST" && operation === "login") {
      const id = String(authSessions.size + 1).padStart(32, "0");
      const session = {
        id,
        host_id: "metal-01",
        name,
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
    if (!session || session.name !== name || session.provider !== provider)
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
          connectedAccounts.add(`${name}/${provider}`);
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
}).listen(18765, "127.0.0.1", () =>
  process.stdout.write("Test fleet API listening on loopback.\n"),
);
