import { spawn } from "node:child_process";
import { mkdir, readFile, writeFile, access } from "node:fs/promises";
import { resolve, join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash, randomBytes } from "node:crypto";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const args = process.argv.slice(2);
const command = args.shift() ?? "up";
const state =
  command === "smoke"
    ? join(root, ".state", `fleet-smoke-${randomBytes(12).toString("hex")}`)
    : resolve(process.env.EBOX_FLEET_STATE ?? join(root, ".state/fleet-dev"));
const name = `ebox-fleet-${createHash("sha256").update(state).digest("hex").slice(0, 10)}`;
const port = Number(
  process.env.EBOX_FLEET_PORT ?? (command === "smoke" ? 18879 : 8799),
);
if (!Number.isInteger(port) || port < 1024 || port > 65535)
  throw new Error("Invalid EBOX_FLEET_PORT");
const env = {
  ...process.env,
  EBOX_FLEET_STATE: state,
  EBOX_FLEET_PORT: String(port),
  EBOX_FLEET_IMAGE: `${name}:local`,
};
const base = [
  "compose",
  "--project-name",
  name,
  "--file",
  join(root, "packages/fleet-dev/compose.yaml"),
];
const url = `http://127.0.0.1:${port}`;
function docker(argv, capture = false) {
  return new Promise((accept, reject) => {
    const child = spawn("docker", [...base, ...argv], {
      cwd: root,
      env,
      stdio: capture ? ["ignore", "pipe", "inherit"] : "inherit",
      windowsHide: true,
    });
    let output = "";
    if (capture)
      child.stdout.on("data", (data) => {
        output += data;
      });
    child.once("error", reject);
    child.once("exit", (code) =>
      code === 0
        ? accept(output.trim())
        : reject(new Error(`Docker Compose exited ${code}`)),
    );
  });
}
async function exists(path) {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}
async function secretFile(path, text) {
  await writeFile(path, text, { mode: 0o600, flag: "wx" });
}
async function adminToken() {
  return JSON.parse(await readFile(join(state, "owner.json"), "utf8")).token;
}
async function api(path, body, token) {
  token ??= await adminToken();
  const response = await fetch(`${url}${path}`, {
    method: body === undefined ? "GET" : "POST",
    headers: {
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(15_000),
  });
  const result = await response.json();
  if (!response.ok) throw new Error(`${response.status}: ${result.error}`);
  return result;
}
async function until(fn, description, milliseconds = 120_000) {
  const deadline = Date.now() + milliseconds;
  while (Date.now() < deadline) {
    const value = await fn();
    if (value) return value;
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`Timed out: ${description}`);
}
async function up(storage) {
  await mkdir(state, { recursive: true, mode: 0o700 });
  if (!(await exists(join(state, "owner.json"))))
    await secretFile(
      join(state, "owner.json"),
      JSON.stringify({ token: randomBytes(32).toString("hex") }),
    );
  const token = await adminToken();
  if (!(await exists(join(state, "controller.dev.vars"))))
    await secretFile(
      join(state, "controller.dev.vars"),
      `ADMIN_TOKEN=${token}\n`,
    );
  await docker(["up", "--build", "--detach", "--wait", "controller"]);
  await api("/v1/projects", { id: "demo" });
  const registered = await api("/v1/nodes");
  for (const id of ["agent-a", "agent-b", "storage"]) {
    const path = join(state, `${id}.json`);
    if (await exists(path)) {
      if (!registered.some((n) => n.id === id))
        throw new Error(
          `Controller state is missing ${id}. Restore its volume, or use a fresh EBOX_FLEET_STATE for an independent fleet.`,
        );
      continue;
    }
    if (registered.some((n) => n.id === id))
      throw new Error(
        `Private enrollment file is missing for ${id}. Restore it from your local backup; secrets cannot be read back from the controller.`,
      );
    const spec = {
      id,
      role: id === "storage" ? "storage" : "agent",
      projects: ["demo"],
      origin: id === "storage" ? "http://storage:8790" : null,
      slots: 1,
      cacheBytes: 256 * 1024 * 1024,
    };
    const enrollment = await api("/v1/nodes", spec);
    await secretFile(
      path,
      JSON.stringify(
        {
          ...spec,
          token: enrollment.token,
          controller: "http://controller:8787",
          state: "/state",
          port: 8790,
        },
        null,
        2,
      ),
    );
  }
  await docker([
    ...(storage ? ["--profile", "storage"] : []),
    "up",
    "--detach",
    "--wait",
    "agent-a",
    "agent-b",
    ...(storage ? ["storage"] : []),
  ]);
  console.log(
    `Local fleet: ${url}/health\nPrivate configuration: ${state}\nProject: demo; two agent nodes${storage ? " and a storage cache" : "; R2 emulator serves shared objects"}.`,
  );
}
async function fixture() {
  const output = await docker(
    [
      "exec",
      "-T",
      "agent-a",
      "node",
      "packages/fleet-node/src/main.ts",
      "fixture",
      "/config/node.json",
    ],
    true,
  );
  return JSON.parse(output).file;
}
async function publish(source) {
  const project = await api("/v1/projects/demo");
  return JSON.parse(
    await docker(
      [
        "exec",
        "-T",
        "agent-a",
        "node",
        "packages/fleet-node/src/main.ts",
        "publish",
        "/config/node.json",
        source,
        "demo",
        project.head ?? "null",
      ],
      true,
    ),
  );
}
async function verify(generation) {
  if (!/^[a-f0-9]{64}$/.test(generation ?? ""))
    throw new Error("A generation SHA-256 is required");
  const id = `verify-${randomBytes(8).toString("hex")}`;
  const started = performance.now();
  await api("/v1/projects/demo/jobs", {
    id,
    project: "demo",
    generation,
    kind: "verify",
  });
  const job = await until(
    async () => {
      const job = await api(`/v1/projects/demo/jobs/${id}`);
      return ["completed", "failed"].includes(job.status) ? job : null;
    },
    "generation verification",
    300_000,
  );
  if (job.status !== "completed")
    throw new Error(`Verification failed: ${job.result}`);
  return {
    job: id,
    node: job.assignment.node,
    seconds: (performance.now() - started) / 1000,
    result: JSON.parse(job.result),
  };
}
async function smoke() {
  await up(true);
  const tasks = JSON.parse(
    await docker(
      [
        "exec",
        "-T",
        "agent-a",
        "node",
        "packages/fleet-node/test/task-mcp-proof.ts",
        "/config/node.json",
        "demo",
      ],
      true,
    ),
  );
  if (!tasks.ok) throw new Error("Task MCP proof failed");
  const source = await fixture();
  const generation = await publish(source);
  const recipe = createHash("sha256")
    .update("enginebox-sqlite-integrity-v1")
    .digest("hex");
  await api("/v1/projects/demo/checks", {
    checks: [{ name: "store-integrity", recipe, kind: "verify" }],
  });
  await api("/v1/projects/demo/candidates", {
    id: "fixture-batch",
    project: "demo",
    base: generation.manifest.parent,
    generation: generation.id,
    changes: [{ id: "fixture-change", generation: generation.id }],
  });
  await docker(["stop", "agent-a"]);
  await api("/v1/projects/demo/jobs", {
    id: `verify-${randomBytes(6).toString("hex")}`,
    project: "demo",
    generation: generation.id,
    kind: "verify",
    check: {
      candidate: "fixture-batch",
      name: "store-integrity",
      recipe,
      scope: "full",
    },
  });
  await until(
    async () =>
      (await api("/v1/projects/demo")).jobs.some(
        (j) => j.spec.generation === generation.id && j.status === "completed",
      ),
    "second-node restore",
  );
  await api("/v1/projects/demo/accept", {
    generation: generation.id,
    expected: generation.manifest.parent,
    candidate: "fixture-batch",
  });
  await until(
    async () =>
      (await api("/v1/nodes")).some(
        (n) => n.id === "agent-b" && n.stats.peerFetches > 0,
      ),
    "storage-node read-through metrics",
  );
  await until(
    async () =>
      (await api("/v1/nodes"))
        .find((n) => n.id === "storage")
        ?.ready.demo?.includes(generation.id),
    "storage generation warmup",
  );
  await docker(["stop", "storage"]);
  await docker(["start", "agent-a"]);
  const second = await publish(await fixture());
  await api("/v1/projects/demo/jobs", {
    id: `fallback-${randomBytes(6).toString("hex")}`,
    project: "demo",
    generation: second.id,
    kind: "verify",
  });
  await until(
    async () =>
      (await api("/v1/projects/demo")).jobs.some(
        (j) => j.spec.generation === second.id && j.status === "completed",
      ),
    "R2 fallback",
  );
  await until(
    async () =>
      (await api("/v1/nodes")).some(
        (n) => n.role === "agent" && n.stats.originFetches > 0,
      ),
    "R2 fallback metrics",
  );
  await docker(["restart", "controller"]);
  await until(async () => {
    try {
      return (await api("/v1/projects/demo")).head === generation.id;
    } catch {
      return false;
    }
  }, "controller state after restart");
  // The original cache stays stopped. A separately enrolled node starts with
  // an empty named volume and must reconstruct the accepted generation from R2.
  const replacement = {
    id: "storage-rebuilt",
    role: "storage",
    projects: ["demo"],
    origin: "http://storage-rebuilt:8790",
    slots: 1,
    cacheBytes: 256 * 1024 * 1024,
  };
  const enrollment = await api("/v1/nodes", replacement);
  await secretFile(
    join(state, "storage-rebuilt.json"),
    JSON.stringify({
      ...replacement,
      token: enrollment.token,
      controller: "http://controller:8787",
      state: "/state",
      port: 8790,
    }),
  );
  await docker([
    "--profile",
    "recovery",
    "up",
    "--detach",
    "--wait",
    "storage-rebuilt",
  ]);
  await until(async () => {
    const rebuilt = (await api("/v1/nodes")).find(
      (n) => n.id === replacement.id,
    );
    return (
      rebuilt?.ready.demo?.includes(generation.id) &&
      rebuilt.stats.originFetches > 0
    );
  }, "empty storage replacement rebuilt from R2");
  console.log(
    JSON.stringify(
      {
        ok: true,
        generation: generation.id,
        snapshotBytes: generation.manifest.snapshotBytes,
        checks: [
          "publish",
          "scoped task MCP create/edit/move",
          "accept",
          "full candidate validation",
          "independent restore",
          "storage reads",
          "warmup",
          "R2 fallback",
          "controller restart",
          "empty storage rebuild",
        ],
      },
      null,
      2,
    ),
  );
}

try {
  if (command === "up") await up(args.includes("--storage"));
  else if (command === "down") await docker(["--profile", "storage", "down"]);
  else if (command === "status") {
    await docker(["--profile", "storage", "ps"]);
    console.log(
      JSON.stringify(
        { projects: await api("/v1/projects"), nodes: await api("/v1/nodes") },
        null,
        2,
      ),
    );
  } else if (command === "logs")
    await docker([
      "--profile",
      "storage",
      "logs",
      "--follow",
      "--tail",
      "100",
      ...args,
    ]);
  else if (command === "smoke") {
    try {
      await smoke();
    } catch (error) {
      await docker([
        "--profile",
        "storage",
        "--profile",
        "recovery",
        "logs",
        "--tail",
        "40",
      ]).catch(() => {});
      throw error;
    } finally {
      await docker([
        "--profile",
        "storage",
        "--profile",
        "recovery",
        "down",
        "--volumes",
      ]);
    }
  } else if (command === "fixture") {
    const generation = await publish(await fixture());
    console.log(
      JSON.stringify(
        { generation, verification: await verify(generation.id) },
        null,
        2,
      ),
    );
  } else if (command === "verify") {
    console.log(JSON.stringify(await verify(args[0]), null, 2));
  } else if (command === "accept") {
    if (!/^[a-f0-9]{64}$/.test(args[0] ?? ""))
      throw new Error("A generation SHA-256 is required");
    const generation = await api(`/v1/projects/demo/generations/${args[0]}`);
    console.log(
      JSON.stringify(
        await api("/v1/projects/demo/accept", {
          generation: args[0],
          expected: generation.manifest.parent,
          ...(args[1] ? { candidate: args[1] } : {}),
        }),
        null,
        2,
      ),
    );
  } else if (command === "publish") {
    if (!args[0])
      throw new Error(
        "Usage: pnpm fleet:publish /path/to/closed-consistent-snapshot.sqlite",
      );
    const destination = `/state/import-${randomBytes(8).toString("hex")}.sqlite`;
    await docker(["cp", resolve(args[0]), `agent-a:${destination}`]);
    try {
      console.log(JSON.stringify(await publish(destination), null, 2));
    } finally {
      await docker([
        "exec",
        "-T",
        "agent-a",
        "node",
        "-e",
        'require("node:fs").unlinkSync(process.argv[1])',
        destination,
      ]);
    }
  } else
    throw new Error(
      "Commands: up [--storage], down, status, logs [service], smoke, fixture, publish SNAPSHOT, verify GENERATION, accept GENERATION",
    );
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
