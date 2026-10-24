// Real local Worker authority for the disposable Docker/WebRTC qualification.
// Never reads a production connection file or uses remote Cloudflare bindings.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomBytes, randomUUID } from "node:crypto";
import { once } from "node:events";
import { createWriteStream } from "node:fs";
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { createRequire } from "node:module";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const directory = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(directory, "../../..");
const controllerRoot = path.join(root, "packages/fleet-controller");
const require = createRequire(path.join(controllerRoot, "package.json"));
const wranglerPackage = require.resolve("wrangler/package.json");
const wranglerManifest = JSON.parse(await readFile(wranglerPackage, "utf8"));
assert.equal(
  wranglerManifest.version,
  "4.152.0",
  "Install the repository-pinned Wrangler",
);
const wranglerBin = path.resolve(
  path.dirname(wranglerPackage),
  wranglerManifest.bin.wrangler,
);
const args = process.argv.slice(2);
assert(
  args.length === 0 || (args.length === 1 && args[0] === "--check-controller"),
  "usage: node test/start-fleet.mjs [--check-controller]",
);
const checkOnly = args[0] === "--check-controller";
const unique = randomBytes(12).toString("hex");
const prefix = `enginebox-stream-proof-${unique}`;
const temporary = await mkdtemp(
  path.join(os.tmpdir(), `enginebox-stream-fleet-${unique}-`),
);
await chmod(temporary, 0o700);
const evidence = path.resolve(
  process.env.STREAM_EVIDENCE_DIR ||
    path.join(root, ".state/stream-fleet-proof", unique),
);
await mkdir(evidence, { recursive: true, mode: 0o700 });
const ownerToken = randomBytes(32).toString("hex");
const project = `stream-proof-${unique}`;
// Docker Desktop can reach loopback through host.docker.internal. Linux Docker
// host-gateway needs the host interface. This is an ephemeral authenticated test
// listener on an isolated development/CI host, never a production authority.
const bind =
  process.platform === "linux" && !checkOnly ? "0.0.0.0" : "127.0.0.1";
const reservation = net.createServer();
await new Promise((resolve, reject) => {
  reservation.once("error", reject);
  reservation.listen(0, bind, resolve);
});
const port = reservation.address().port;
await new Promise((resolve) => reservation.close(resolve));
const controller = `http://127.0.0.1:${port}`;
const dockerController = `http://host.docker.internal:${port}`;
const connectionFile = path.join(temporary, "connection.private.json");
const logFile = path.join(temporary, "controller.log");
const log = createWriteStream(logFile, { mode: 0o600 });
const children = new Set();
let interrupted = false;
let runtime;
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms).unref());

function child(command, arguments_, options = {}) {
  const proc = spawn(command, arguments_, {
    detached: process.platform !== "win32",
    ...options,
  });
  children.add(proc);
  proc.once("error", (error) => {
    proc.launchError = error;
  });
  return proc;
}
function signal(proc, name) {
  if (!proc || proc.exitCode !== null || proc.signalCode !== null) return;
  try {
    if (process.platform === "win32") proc.kill(name);
    else process.kill(-proc.pid, name);
  } catch (error) {
    if (error.code !== "ESRCH") throw error;
  }
}
async function stop(proc) {
  if (!proc || proc.exitCode !== null || proc.signalCode !== null) return;
  if (process.platform === "win32") {
    // Killing only node.exe would strand its workerd/browser descendants.
    await command("taskkill", ["/PID", String(proc.pid), "/T", "/F"]).catch(
      () => {},
    );
    if (proc.exitCode === null && proc.signalCode === null)
      await Promise.race([once(proc, "exit").catch(() => {}), pause(3000)]);
    return;
  }
  signal(proc, "SIGTERM");
  await Promise.race([once(proc, "exit").catch(() => {}), pause(15000)]);
  if (proc.exitCode === null && proc.signalCode === null) {
    if (process.platform === "win32") {
      await command("taskkill", ["/PID", String(proc.pid), "/T", "/F"]).catch(
        () => {},
      );
    } else signal(proc, "SIGKILL");
    await Promise.race([once(proc, "exit").catch(() => {}), pause(3000)]);
  }
}
function command(program, arguments_) {
  return new Promise((resolve, reject) => {
    const proc = spawn(program, arguments_, { stdio: "ignore" });
    const timeout = setTimeout(() => {
      proc.kill("SIGKILL");
      reject(Error(`${program} cleanup timed out`));
    }, 20000);
    proc.once("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
    proc.once("close", (code) => {
      clearTimeout(timeout);
      code === 0 ? resolve() : reject(Error(`${program} cleanup failed`));
    });
  });
}
const onInterrupt = () => {
  interrupted = true;
  if (process.platform === "win32") void stop(runtime);
  else {
    signal(runtime, "SIGTERM");
    setTimeout(() => signal(runtime, "SIGKILL"), 15000).unref();
  }
};
process.on("SIGINT", onInterrupt);
process.on("SIGTERM", onInterrupt);

try {
  await writeFile(
    path.join(temporary, "wrangler.jsonc"),
    JSON.stringify({
      name: `stream-proof-${unique}`,
      main: path.join(controllerRoot, "src/index.ts"),
      compatibility_date: "2026-10-19",
      compatibility_flags: ["nodejs_compat"],
      // With this Wrangler version, required also filters local .dev.vars.
      // This fixture always supplies both; production TURN remains optional.
      secrets: { required: ["ADMIN_TOKEN", "MACHINE_TURN_CONFIG"] },
      vars: { ALLOW_HTTP_NODES: "true" },
      durable_objects: {
        bindings: [
          ["REGISTRY", "Registry"],
          ["NODES", "FleetNode"],
          ["PROJECTS", "Project"],
        ].map(([name, class_name]) => ({ name, class_name })),
      },
      migrations: [
        { tag: "v1", new_sqlite_classes: ["Registry", "FleetNode", "Project"] },
      ],
      r2_buckets: [{ binding: "STORE", bucket_name: `stream-proof-${unique}` }],
    }),
    { mode: 0o600 },
  );
  // These are public fixture-only coturn credentials from relay.Dockerfile.
  // Single-quote JSON for dotenv; escaped double-quoted JSON changes its value.
  const turn = JSON.stringify({
    local: true,
    iceServers: [
      {
        urls: ["turn:relay:3478?transport=udp"],
        username: "fixture",
        credential: "fixture-only",
      },
    ],
    iceTransportPolicy: "relay",
  });
  await writeFile(
    path.join(temporary, ".dev.vars"),
    `ADMIN_TOKEN=${ownerToken}\nMACHINE_TURN_CONFIG='${turn}'\n`,
    { mode: 0o600 },
  );
  await writeFile(
    connectionFile,
    JSON.stringify({ controller, dockerController, project, ownerToken }),
    { mode: 0o600 },
  );
  const cleanEnv = Object.fromEntries(
    Object.entries(process.env).filter(
      ([key]) =>
        !/(TOKEN|SECRET|API_KEY|CLOUDFLARE|ACCOUNT_ID|TURN_KEY)/i.test(key),
    ),
  );
  cleanEnv.CI = "1";
  cleanEnv.WRANGLER_SEND_METRICS = "false";
  const authority = child(
    process.execPath,
    [
      wranglerBin,
      "dev",
      "--local",
      "--config",
      path.join(temporary, "wrangler.jsonc"),
      "--persist-to",
      path.join(temporary, "state"),
      "--ip",
      bind,
      "--port",
      String(port),
      "--inspector-port",
      "0",
      "--show-interactive-dev-session=false",
    ],
    { cwd: temporary, env: cleanEnv, stdio: ["ignore", log, log] },
  );
  const deadline = Date.now() + 60000;
  let healthy = false;
  while (Date.now() < deadline && !interrupted) {
    if (authority.launchError) throw authority.launchError;
    assert(
      authority.exitCode === null && authority.signalCode === null,
      "Disposable Wrangler exited before readiness",
    );
    try {
      const response = await fetch(`${controller}/health`, {
        signal: AbortSignal.timeout(1000),
      });
      healthy =
        response.ok && (await response.json()).service === "enginebox-fleet";
      if (healthy) break;
    } catch {}
    await pause(250);
  }
  assert(healthy && !interrupted, "Disposable Wrangler did not become ready");
  const create = await fetch(`${controller}/v1/projects`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${ownerToken}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ id: project }),
    signal: AbortSignal.timeout(10000),
  });
  assert.equal(
    create.status,
    201,
    "Fresh owner must create only the disposable project",
  );
  const denied = await fetch(`${controller}/v1/projects`, {
    signal: AbortSignal.timeout(5000),
  });
  assert.equal(
    denied.status,
    401,
    "Disposable controller must deny anonymous authority",
  );
  const call = async (route, body, token = ownerToken, headers = {}) => {
    const response = await fetch(`${controller}${route}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
        ...headers,
      },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(10000),
    });
    assert(
      response.ok,
      `Disposable authority preflight ${route.split("/").at(-1)} HTTP${response.status}`,
    );
    return response.json();
  };
  // Authority-only probe: exercise local relay configuration before starting
  // Docker, without claiming this synthetic heartbeat is a working media host.
  const probe = `probe-${unique}`,
    base = `/v1/projects/${project}`;
  const invitation = await call(`${base}/machine-invitations`, {
    id: probe,
    label: "Disposable relay configuration probe",
    ownerAccountId: "fixture-probe",
    allowControl: false,
  });
  try {
    const nativeToken = `${probe}.${randomBytes(32).toString("hex")}`;
    await call(
      "/v1/node/enroll",
      {
        nodeSecretHash: createHash("sha256").update(nativeToken).digest("hex"),
      },
      invitation.token,
    );
    const session = randomUUID();
    const hello = await call("/v1/node/hello", { session }, nativeToken);
    await call(
      "/v1/node/heartbeat",
      { stats: { webrtc: 1, video: 1 } },
      nativeToken,
      {
        "x-enginebox-epoch": String(hello.epoch),
        "x-enginebox-session": session,
      },
    );
    const grant = await call(`${base}/machine-session-grants`, {
      id: `grant-${unique}`,
      sessionId: `view-${unique}`,
      nodeId: probe,
      actorId: "fixture-probe",
      mode: "watch",
    });
    assert.equal(
      grant.nodeId,
      probe,
      "Local relay grant must retain native scope",
    );
  } finally {
    await call(`${base}/machines/${probe}/revoke`, {
      operationId: "probe-cleanup",
    });
  }
  if (checkOnly) {
    await writeFile(
      path.join(evidence, "controller-check.json"),
      JSON.stringify(
        {
          passed: true,
          realController: true,
          localBindings: true,
          project,
          port,
          anonymousDenied: true,
          localRelayGrant: true,
        },
        null,
        2,
      ),
    );
  } else {
    runtime = child(process.execPath, [path.join(directory, "run.mjs")], {
      cwd: root,
      env: {
        ...process.env,
        STREAM_CONTROLLER_CONFIG: connectionFile,
        STREAM_EVIDENCE_DIR: evidence,
        STREAM_TEST_PREFIX: prefix,
      },
      stdio: "inherit",
    });
    const [code, termination] = await once(runtime, "exit");
    assert(
      !interrupted && code === 0 && !termination,
      "Real fleet WebRTC qualification failed or was interrupted",
    );
    const receipt = JSON.parse(
      await readFile(path.join(evidence, "result.json"), "utf8"),
    );
    assert(receipt.passed, "Missing passing media receipt");
    assert.equal(
      receipt.controller,
      "real isolated Wrangler fleet",
      "Protocol-only fixture is insufficient for this qualification",
    );
  }
  console.log(
    JSON.stringify({
      passed: true,
      mode: checkOnly ? "controller-check" : "real-fleet-webrtc",
      controller,
      project,
      evidence,
    }),
  );
} finally {
  for (const proc of [...children].reverse()) await stop(proc);
  // run.mjs cleans normally. These exact random names are the interruption
  // fallback; never enumerate/prune Docker or touch another test's resources.
  if (!checkOnly && runtime) {
    for (const suffix of ["host", "driver", "relay"])
      await command("docker", ["rm", "-f", `${prefix}-${suffix}`]).catch(
        () => {},
      );
    await command("docker", ["volume", "rm", `${prefix}-state`]).catch(
      () => {},
    );
    await command("docker", ["network", "rm", `${prefix}-net`]).catch(() => {});
  }
  await new Promise((resolve) => log.end(resolve));
  await copyFile(logFile, path.join(evidence, "controller.log")).catch(
    () => {},
  );
  await chmod(path.join(evidence, "controller.log"), 0o600).catch(() => {});
  await rm(temporary, { recursive: true, force: true });
  await writeFile(
    path.join(evidence, "controller-cleanup.json"),
    JSON.stringify(
      {
        localControllerStopped: [...children].every(
          (proc) => proc.exitCode !== null || proc.signalCode !== null,
        ),
        privateConnectionRemoved: true,
        localDoR2StateRemoved: true,
        dockerCleanup: checkOnly ? "not-used" : "exact-owned-names-only",
        interrupted,
      },
      null,
      2,
    ),
  );
  process.off("SIGINT", onInterrupt);
  process.off("SIGTERM", onInterrupt);
}
