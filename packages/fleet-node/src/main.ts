import { createServer, type IncomingMessage } from "node:http";
import { mkdir, readFile, writeFile, open, rm } from "node:fs/promises";
import { resolve, join } from "node:path";
import { randomUUID, createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";
import {
  canonical,
  digest,
  identifier,
  integer,
  ProtocolError,
  requireThat,
  readOnlyJob,
  type Assignment,
  type NodeSpec,
} from "@enginebox/fleet-protocol";
import { executeIntegration, type IntegrationConfig } from "./integration.ts";
import { EngineBuilds, type EngineBuildConfig } from "./build.ts";
import { executeMedia, type MediaConfig } from "./media.ts";
import { Cache } from "./cache.ts";
import { Client } from "./client.ts";
import { Generations, publishSnapshot, type Published } from "./generations.ts";
import {
  NativeAssistant,
  UncertainNativeRun,
  type NativeAssistantConfig,
} from "./native-assistant.ts";

export type Config = NodeSpec & {
  controller: string;
  token: string;
  state: string;
  port: number;
  snapshotBytes?: number;
  warmDelayMs?: number;
  nativeAssistant?: NativeAssistantConfig;
  media?: MediaConfig;
  integration?: IntegrationConfig;
  engineBuilds?: EngineBuildConfig;
  layerfs?: {
    ebox: string;
    config: string;
    bindings: Record<
      string,
      { project: string; workspace: string; generation: string }
    >;
  };
};
type Catalog = {
  project: string;
  head: string | null;
  generations: { id: string; sequence: number; snapshotBytes: number }[];
}[];
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
function log(event: string, data: Record<string, unknown> = {}) {
  console.log(
    JSON.stringify({ time: new Date().toISOString(), event, ...data }),
  );
}
async function verifyGrant(token: string, secret: string, epoch: number) {
  const [payload, signature, ...extra] = token.split(".");
  requireThat(
    payload &&
      signature &&
      extra.length === 0 &&
      /^[a-f0-9]{64}$/.test(signature),
    "Invalid transfer grant",
    401,
  );
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(createHash("sha256").update(secret).digest("hex")),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["verify"],
  );
  requireThat(
    await crypto.subtle.verify(
      "HMAC",
      key,
      Buffer.from(signature, "hex"),
      new TextEncoder().encode(payload),
    ),
    "Invalid transfer grant",
    401,
  );
  const grant = JSON.parse(Buffer.from(payload, "base64").toString("utf8"));
  identifier(grant.project);
  digest(grant.generation);
  requireThat(
    grant.expires > Date.now() &&
      grant.expires <= Date.now() + 65_000 &&
      grant.epoch === epoch,
    "Transfer grant expired",
    401,
  );
  return grant as {
    project: string;
    generation: string;
    reader: string;
    expires: number;
    epoch: number;
  };
}
function errorStatus(error: unknown) {
  const match = (error instanceof Error ? error.message : "").match(
    /\[EBOX (\d{3})\]/,
  );
  return match ? Number(match[1]) : 500;
}

export async function runNode(config: Config) {
  identifier(config.id);
  requireThat(
    !config.taskOnly,
    "Task-only credentials cannot run a fleet node",
    403,
  );
  requireThat(
    config.role === "agent" || config.role === "storage",
    "Invalid role",
  );
  integer(config.cacheBytes, 16 * 1024 * 1024, Number.MAX_SAFE_INTEGER);
  await mkdir(config.state, { recursive: true, mode: 0o700 });
  const lock = new DatabaseSync(join(config.state, "node-lock.sqlite"));
  try {
    lock.exec(
      "PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; CREATE TABLE IF NOT EXISTS owner(id INTEGER)",
    );
  } catch {
    lock.close();
    throw new ProtocolError(
      409,
      "Node state is already owned by another process",
    );
  }
  const cache = new Cache(config.state, config.cacheBytes);
  cache.clearPins();
  await cache.cleanPartials();
  const client = new Client(config.controller, config.token);
  client.session = randomUUID();
  const generations = new Generations(
    client,
    cache,
    config.state,
    config.snapshotBytes,
  );
  await generations.snapshots.recover();
  const journal = new DatabaseSync(join(config.state, "operations.sqlite"));
  journal.exec(
    "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS operations(attempt TEXT PRIMARY KEY, state TEXT NOT NULL, assignment TEXT NOT NULL, result TEXT)",
  );
  const nativeAssistant = config.nativeAssistant
    ? new NativeAssistant(config.nativeAssistant, journal)
    : null;
  requireThat(
    canonical([...(config.assistantProjects ?? [])].sort()) ===
      canonical(Object.keys(config.nativeAssistant?.bindings ?? {}).sort()),
    "Enrolled assistant projects must match configured native bindings",
  );
  for (const [capabilities, bindings, kind] of [
    [
      config.integrationProjects ?? [],
      Object.keys(config.integration?.projects ?? {}),
      "integration",
    ],
    [
      config.buildProjects ?? [],
      Object.keys(config.engineBuilds?.bindings ?? {}),
      "build",
    ],
    [config.mediaProjects ?? [], config.media?.projects ?? [], "media"],
  ] as [string[], string[], string][])
    requireThat(
      canonical([...capabilities].sort()) === canonical([...bindings].sort()),
      `Enrolled ${kind} projects must match configured bindings`,
    );
  const engineBuilds = config.engineBuilds
    ? new EngineBuilds(config.engineBuilds, client)
    : null;
  let blockedNative = false;
  let stopping = false;
  let connected = false;
  let warming = false;
  let active: Assignment | null = null;
  let lastCatalog: Catalog = [];
  let lastError: string | null = null;
  const ready = new Map<string, Set<string>>();
  let abortJob: AbortController | null = null;
  const watchdog = setInterval(() => {
    if (active && active.expires <= Date.now()) abortJob?.abort();
  }, 250);
  let serving = 0;
  const server = createServer(async (request, response) => {
    let transfer = false;
    try {
      const url = new URL(request.url ?? "/", "http://node");
      if (request.method === "GET" && url.pathname === "/health") {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(
          JSON.stringify({
            ok: true,
            connected,
            node: config.id,
            role: config.role,
          }),
        );
        return;
      }
      if (config.role === "storage" && request.method === "GET") {
        requireThat(serving < 8, "Storage transfer capacity reached", 503);
        serving++;
        transfer = true;
        const parts = url.pathname.split("/").filter(Boolean);
        requireThat(
          parts.length === 4 && parts[0] === "v1" && parts[1] === "chunks",
          "Unknown storage route",
          404,
        );
        const grant = await verifyGrant(
          request.headers.authorization?.replace(/^Bearer /, "") ?? "",
          config.token,
          client.epoch,
        );
        requireThat(
          config.projects.includes(grant.project) && parts[2] === grant.project,
          "Project access denied",
          403,
        );
        digest(parts[3]);
        const { chunks } = await generations.index(
          grant.project,
          grant.generation,
        );
        const chunk = chunks.find((c) => c.hash === parts[3]);
        requireThat(chunk, "Chunk is not part of authorized generation", 403);
        const hits = cache.stats.hits;
        const bytes = await generations.chunk(
          grant.project,
          grant.generation,
          chunk,
          false,
        );
        response.writeHead(200, {
          "content-type": "application/octet-stream",
          "content-length": bytes.length,
          "x-enginebox-cache": cache.stats.hits > hits ? "hit" : "miss",
          etag: chunk.hash,
        });
        response.end(bytes);
        return;
      }
      throw new ProtocolError(404, "Unknown node route");
    } catch (error) {
      const status = errorStatus(error);
      response.writeHead(status, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          error:
            status === 500 ? "Node operation failed" : (error as Error).message,
        }),
      );
    } finally {
      if (transfer) serving--;
    }
  });
  await new Promise<void>((accept, reject) => {
    server.once("error", reject);
    server.listen(config.port, "0.0.0.0", accept);
  });
  async function execute(assignment: Assignment) {
    if (active) return;
    const prior = journal
      .prepare("SELECT state,result FROM operations WHERE attempt=?")
      .get(assignment.attempt) as
      | { state: string; result: string | null }
      | undefined;
    active = assignment;
    abortJob = new AbortController();
    const signal = abortJob.signal;
    let result: Record<string, unknown>;
    let ok = false;
    try {
      if (prior?.state === "finished") {
        const saved = JSON.parse(prior.result!);
        ok = saved.ok;
        result = saved.result;
      } else {
        if (prior && !readOnlyJob(assignment))
          throw new UncertainNativeRun(
            "Interrupted native command requires explicit reconciliation",
          );
        journal
          .prepare("INSERT OR REPLACE INTO operations VALUES(?,?,?,NULL)")
          .run(assignment.attempt, "running", canonical(assignment));
        if (assignment.kind === "assistant") {
          requireThat(
            nativeAssistant,
            "Native assistant is not configured",
            409,
          );
          result = await nativeAssistant.execute(assignment, signal);
        } else if (assignment.kind === "integration") {
          requireThat(
            config.integration,
            "Integration runtime is not configured",
            409,
          );
          result = await executeIntegration(
            config.integration,
            assignment,
            client,
            config.state,
            signal,
          );
        } else if (assignment.kind === "engine-build") {
          requireThat(
            engineBuilds,
            "Protected engine builds are not configured",
            409,
          );
          result = await engineBuilds.execute(assignment, signal);
        } else if (assignment.kind === "media-finalize") {
          requireThat(config.media, "Media validation is not configured", 409);
          result = await executeMedia(
            config.media,
            assignment,
            client,
            config.state,
            signal,
          );
        } else {
          await generations.hydrate(assignment.project, assignment.generation);
          requireThat(
            !signal.aborted && assignment.expires > Date.now(),
            "Job lease expired",
            409,
          );
          if (assignment.kind === "layerfs-exec")
            result = await nativeCommand(config, assignment, signal);
          else
            result = {
              generation: assignment.generation,
              verified: true,
              snapshotBytes: (
                await generations.index(
                  assignment.project,
                  assignment.generation,
                )
              ).published.manifest.snapshotBytes,
            };
        }
        requireThat(
          !signal.aborted && assignment.expires > Date.now(),
          "Job lease expired",
          409,
        );
        ok = true;
        if (assignment.check) result.check = assignment.check;
      }
    } catch (error) {
      if (error instanceof UncertainNativeRun) {
        blockedNative = true;
        log("native_reconciliation_required", {
          job: assignment.id,
          attempt: assignment.attempt,
          error: error.message,
        });
        active = null;
        abortJob = null;
        return;
      }
      result = { error: (error as Error).message };
    }
    journal
      .prepare("INSERT OR REPLACE INTO operations VALUES(?,?,?,?)")
      .run(
        assignment.attempt,
        "finished",
        canonical(assignment),
        canonical({ ok, result }),
      );
    try {
      await client.json("/v1/node/finish", { assignment, ok, result });
      log("job_finished", { job: assignment.id, ok });
    } catch (error) {
      log("result_pending", {
        job: assignment.id,
        error: (error as Error).message,
      });
    } finally {
      active = null;
      abortJob = null;
    }
  }
  async function warm() {
    if (warming || config.role !== "storage") return;
    warming = true;
    try {
      for (const project of lastCatalog) {
        const selected = [...project.generations]
          .sort(
            (a, b) =>
              Number(b.id === project.head) - Number(a.id === project.head) ||
              b.sequence - a.sequence,
          )
          .filter((g) => g.snapshotBytes <= config.cacheBytes * 0.7)
          .slice(0, 1);
        const keep = new Set(selected.map((g) => g.id));
        const owned = ready.get(project.project) ?? new Set<string>();
        for (const old of [...owned])
          if (!keep.has(old)) {
            await client.json("/v1/node/ready", {
              project: project.project,
              generation: old,
              ready: false,
            });
            cache.unpin(`generation:${project.project}:${old}`);
            owned.delete(old);
          }
        ready.set(project.project, owned);
        for (const generation of selected) {
          if (stopping) break;
          if (owned.has(generation.id)) continue;
          if (config.warmDelayMs) await sleep(config.warmDelayMs);
          await generations.warm(project.project, generation.id);
          await client.json("/v1/node/ready", {
            project: project.project,
            generation: generation.id,
            ready: true,
          });
          owned.add(generation.id);
          log("generation_ready", {
            project: project.project,
            generation: generation.id,
          });
        }
      }
    } catch (error) {
      log("warmup_pending", { error: (error as Error).message });
    } finally {
      warming = false;
    }
  }
  async function loop() {
    let tick = 0;
    while (!stopping) {
      try {
        if (!connected) {
          const hello = await client.json<{
            epoch: number;
            session: string;
            reservations: Assignment[];
          }>("/v1/node/hello", { session: client.session });
          client.epoch = hello.epoch;
          for (const reservation of hello.reservations) {
            if (
              !readOnlyJob(reservation) &&
              (reservation.epoch !== client.epoch ||
                reservation.expires <= Date.now())
            )
              blockedNative = true;
            else if (
              readOnlyJob(reservation) &&
              reservation.epoch !== client.epoch
            )
              await client.json("/v1/node/reconcile", {
                attempt: reservation.attempt,
              });
          }
          connected = true;
          log("node_connected", { node: config.id, epoch: client.epoch });
        }
        await client.json("/v1/node/heartbeat", {
          stats: {
            ...cache.stats,
            ...generations.stats,
            cacheBytes: cache.bytes(),
            activeJobs: active ? 1 : 0,
            uncertainNativeJobs: blockedNative ? 1 : 0,
          },
        });
        if (active) {
          const renewing = active;
          try {
            const lease = await client.json<{ expires: number }>(
              "/v1/node/renew",
              { assignment: renewing },
            );
            if (active?.attempt === renewing.attempt)
              active.expires = lease.expires;
          } catch (error) {
            if (active && active.expires <= Date.now()) abortJob?.abort();
            throw error;
          }
        }
        if (tick++ % 2 === 0) {
          lastCatalog = await client.json<Catalog>("/v1/node/catalog");
          void warm();
        }
        if (config.role === "agent" && !active && !blockedNative) {
          const { assignment, reservations } = await client.json<{
            assignment: Assignment | null;
            reservations?: Assignment[];
          }>("/v1/node/poll", {});
          if (
            reservations?.some(
              (reserved) =>
                !readOnlyJob(reserved) &&
                (reserved.epoch !== client.epoch ||
                  reserved.expires <= Date.now()),
            )
          )
            blockedNative = true;
          for (const reserved of reservations ?? [])
            if (readOnlyJob(reserved) && reserved.expires <= Date.now())
              await client.json("/v1/node/reconcile", {
                attempt: reserved.attempt,
              });
          if (assignment)
            void execute(assignment).catch((error) => {
              log("executor_failed", { error: error.message });
              active = null;
              abortJob = null;
            });
        }
        lastError = null;
      } catch (error) {
        lastError = (error as Error).message;
        log("reconcile_pending", { error: lastError });
        if (active && active.expires <= Date.now()) abortJob?.abort();
      }
      await sleep(2000);
    }
  }
  const running = loop();
  async function stop() {
    if (stopping) return;
    stopping = true;
    clearInterval(watchdog);
    abortJob?.abort();
    const closed = new Promise<void>((accept) => server.close(() => accept()));
    server.closeAllConnections();
    await running;
    while (active || warming) await sleep(50);
    await closed;
    await Promise.allSettled([...cache.inflight.values()]);
    cache.close();
    journal.close();
    lock.close();
  }
  process.once("SIGINT", () => void stop());
  process.once("SIGTERM", () => void stop());
  return { stop, client, cache, generations };
}

async function nativeCommand(
  config: Config,
  assignment: Assignment,
  signal: AbortSignal,
): Promise<Record<string, unknown>> {
  const runtime = config.layerfs;
  requireThat(runtime, "LayerFS execution is not configured on this node", 409);
  const binding = runtime.bindings[assignment.project];
  requireThat(
    binding && binding.generation === assignment.generation,
    "No qualified LayerFS base for this generation",
    409,
  );
  identifier(binding.project);
  identifier(binding.workspace);
  const workspace = `task-${assignment.attempt.replaceAll("-", "").slice(0, 24)}`;
  const prefix = [
    "--host",
    "local",
    "storage",
    "layerfs",
    "--config",
    runtime.config,
  ];
  async function invoke(args: string[]) {
    requireThat(!signal.aborted, "Lease expired", 409);
    return new Promise<Record<string, unknown>>((resolve, reject) => {
      const child = spawn(runtime!.ebox, [...prefix, ...args], {
        stdio: ["ignore", "pipe", "pipe"],
        signal,
        windowsHide: true,
      });
      let output = "";
      let errors = "";
      child.stdout.on("data", (data) => {
        output += data.toString();
        if (output.length > 1024 * 1024) child.kill();
      });
      child.stderr.on("data", (data) => {
        errors = (errors + data.toString()).slice(-64 * 1024);
      });
      child.once("error", reject);
      child.once("exit", (code) => {
        if (code !== 0)
          reject(
            new Error(`Native LayerFS operation failed (${code}): ${errors}`),
          );
        else {
          try {
            const parsed = JSON.parse(output);
            requireThat(
              parsed.ok === true,
              "Native adapter reported an unsuccessful operation",
              409,
            );
            resolve(parsed);
          } catch {
            reject(new Error("Native adapter returned invalid JSON"));
          }
        }
      });
    });
  }
  await invoke([
    "fork",
    binding.project,
    workspace,
    "--from",
    binding.workspace,
  ]);
  const result = await invoke([
    "exec",
    workspace,
    "--timeout",
    String(assignment.timeoutSeconds),
    "--",
    ...assignment.argv!,
  ]);
  await invoke([
    "checkpoint",
    workspace,
    "--message",
    `Fleet task ${assignment.id}`,
  ]);
  return { workspace, result, checkpointed: true };
}

async function main() {
  const [command, ...args] = process.argv.slice(2);
  const configPath = args[0];
  requireThat(
    configPath,
    "Usage: main.ts serve|publish|fixture CONFIG [arguments]",
  );
  const config = JSON.parse(await readFile(configPath, "utf8")) as Config;
  if (command === "serve") {
    await runNode(config);
    return;
  }
  if (command === "publish") {
    const [source, project, parent = "null"] = args.slice(1);
    identifier(project);
    const published = await publishSnapshot(
      new Client(config.controller, config.token),
      project,
      resolve(source),
      join(config.state, "publications"),
      parent === "null" ? null : parent,
    );
    console.log(JSON.stringify(published));
    return;
  }
  if (command === "fixture") {
    const file = join(config.state, "fixture.sqlite");
    await mkdir(config.state, { recursive: true, mode: 0o700 });
    const db = new DatabaseSync(file);
    db.exec(
      "CREATE TABLE IF NOT EXISTS fixture(id INTEGER PRIMARY KEY, data BLOB)",
    );
    const insert = db.prepare("INSERT INTO fixture(data) VALUES (?)");
    for (let i = 0; i < 8; i++) insert.run(Buffer.alloc(1024 * 1024, i));
    db.close();
    console.log(JSON.stringify({ file }));
    return;
  }
  throw new Error("Unknown command");
}
if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
)
  main().catch((error) => {
    console.error(JSON.stringify({ error: error.message }));
    process.exitCode = 1;
  });
