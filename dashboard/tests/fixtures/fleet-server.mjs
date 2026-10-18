// Test-only API fixture. The application never imports this module.
import { createServer } from "node:http";
const GiB = 1024 ** 3;
const token = "fixture-api-token-server-only-12345";
let mode = "normal";
let jobs = new Map();
let instances;
function reset() {
  mode = "normal";
  jobs = new Map();
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
    return send(200, { ok: true });
  }
  if (req.headers.authorization !== `Bearer ${token}`)
    return send(401, { error: "Wrong backend token" });
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
                  max_additional: { small: 8, standard: 4, large: 2 },
                },
                instances,
              },
              {
                id: "metal-02",
                label: "Spare machine",
                online: false,
                error: "SSH connection timed out.",
                instances: [],
              },
            ],
      templates: [{ id: "native", label: "Native coding agents" }],
      sizes: {
        small: { cpu: 2, memory_gib: 2 },
        standard: { cpu: 4, memory_gib: 4 },
        large: { cpu: 8, memory_gib: 8 },
      },
    });
  }
  const action = req.url.match(
    /^\/v1\/hosts\/([^/]+)\/instances\/([^/]+)\/actions$/,
  );
  if (
    req.method === "POST" &&
    (req.url === "/v1/hosts/metal-01/instances" || action)
  ) {
    const id = `job-${jobs.size + 1}`;
    const job = {
      id,
      status: "queued",
      polls: 0,
      name: action?.[2] ?? data.name,
      action: data.action ?? "create",
      data,
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
        if (job.action === "create")
          instances.push({
            name: job.name,
            status: "Stopped",
            stateful: false,
            ipv4: [],
            cpu: 2,
            memory_bytes: 2 * GiB,
            managed: true,
          });
        else {
          const vm = instances.find((vm) => vm.name === job.name);
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
      job: { id: job.id, status: job.status, error: job.error },
    });
  }
  send(404, { error: "Not found" });
}).listen(18765, "127.0.0.1", () =>
  process.stdout.write("Test fleet API listening on loopback.\n"),
);
