import { timingSafeEqual } from "node:crypto";
import {
  canonical,
  digest,
  identifier,
  integer,
  jsonBody,
  MAX_CHUNK_BYTES,
  MAX_JSON_BYTES,
  NODE_STALE_MS,
  ProtocolError,
  readBounded,
  record,
  requireThat,
  readOnlyJob,
  sha256,
  validateChunks,
  validateGeneration,
  validateJob,
  validateNode,
  validateCandidate,
  validateChecks,
  validateTaskCreate,
  validateTaskUpdate,
  validateTaskMove,
  taskStatus,
  type Assignment,
  type NodeView,
} from "@enginebox/fleet-protocol";
import { assistantRoute } from "./assistant.ts";
export { Registry, FleetNode, Project } from "./state.ts";

const json = (value: unknown, status = 200) =>
  Response.json(value, {
    status,
    headers: {
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
    },
  });
type Principal = { admin: true } | { admin: false; node: NodeView };
function visibleInventory<T extends { jobs: { spec: { kind: string } }[] }>(
  p: Principal,
  inventory: T,
) {
  if (p.admin || p.node.role === "agent") return inventory;
  return {
    ...inventory,
    jobs: inventory.jobs.filter((job) => job.spec.kind !== "assistant"),
  };
}
function bearer(request: Request) {
  return (
    request.headers.get("authorization")?.match(/^Bearer ([^\s]+)$/)?.[1] ?? ""
  );
}
async function principal(request: Request, env: Env): Promise<Principal> {
  const token = bearer(request);
  requireThat(token.length >= 32, "Authorization required", 401);
  requireThat(
    env.ADMIN_TOKEN && env.ADMIN_TOKEN.length >= 32,
    "Controller secret is not configured",
    503,
  );
  const a = new TextEncoder().encode(await sha256(token));
  const b = new TextEncoder().encode(await sha256(env.ADMIN_TOKEN));
  if (timingSafeEqual(a, b)) return { admin: true };
  const id = token.split(".")[0];
  identifier(id);
  return {
    admin: false,
    node: await env.NODES.getByName(id).authenticate(await sha256(token)),
  };
}
function admin(p: Principal) {
  requireThat(p.admin, "Owner authorization required", 403);
}
function node(p: Principal) {
  requireThat(!p.admin, "Node authorization required", 403);
  return p.node;
}
function projectAccess(p: Principal, project: string) {
  identifier(project);
  requireThat(
    p.admin || p.node.projects.includes(project),
    "Project access denied",
    403,
  );
}
function session(request: Request) {
  const epoch = Number(request.headers.get("x-enginebox-epoch"));
  integer(epoch, 1, Number.MAX_SAFE_INTEGER);
  const value = request.headers.get("x-enginebox-session");
  requireThat(
    typeof value === "string" && /^[a-f0-9-]{36}$/.test(value),
    "Node session required",
  );
  return { epoch, session: value };
}
async function requireProject(env: Env, project: string) {
  requireThat(
    (await env.PROJECTS.getByName(project).identity()) === project,
    "Unknown project",
    404,
  );
}
function part(parts: string[], index: number) {
  const value = parts[index];
  requireThat(value, "Missing path component");
  return value;
}
async function mapLimit<T, U>(
  items: T[],
  concurrency: number,
  fn: (item: T) => Promise<U>,
): Promise<U[]> {
  const results = new Array<U>(items.length);
  let cursor = 0;
  await Promise.all(
    Array.from({ length: Math.min(concurrency, items.length) }, async () => {
      for (;;) {
        const index = cursor++;
        if (index >= items.length) return;
        results[index] = await fn(items[index]);
      }
    }),
  );
  return results;
}
async function immutablePut(
  env: Env,
  key: string,
  bytes: Uint8Array | string,
  metadata: Record<string, string>,
) {
  const object = await env.STORE.put(key, bytes, {
    onlyIf: { etagDoesNotMatch: "*" },
    customMetadata: metadata,
  });
  if (!object) {
    const existing = await env.STORE.head(key);
    requireThat(
      existing &&
        Object.entries(metadata).every(
          ([k, v]) => existing.customMetadata?.[k] === v,
        ),
      "Existing immutable object metadata differs",
      409,
    );
  }
}
async function assignmentBody(request: Request, p: Principal) {
  const n = node(p);
  const active = session(request);
  const body = await jsonBody(request);
  record(body);
  record(body.assignment);
  const a = body.assignment as unknown as Assignment;
  identifier(a.id);
  identifier(a.project);
  if (a.kind === "assistant")
    requireThat(
      a.generation === null,
      "Assistant jobs have no Store generation",
    );
  else digest(a.generation);
  requireThat(
    a.node === n.id &&
      a.epoch === active.epoch &&
      a.session === active.session &&
      n.epoch === active.epoch &&
      n.session === active.session &&
      typeof a.attempt === "string",
    "Assignment does not belong to session",
    403,
  );
  projectAccess(p, a.project);
  return { n, active, body, assignment: a };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    try {
      const url = new URL(request.url);
      const parts = url.pathname.split("/").filter(Boolean);
      if (request.method === "GET" && url.pathname === "/health")
        return json({ ok: true, service: "enginebox-fleet", schema: 1 });
      if (request.method === "POST" && url.pathname === "/v1/node/enroll") {
        const token = bearer(request);
        const match = /^([a-z0-9-]{1,48})\.enroll\.([a-f0-9]{64})$/.exec(token);
        requireThat(match, "Enrollment credential required", 401);
        identifier(match[1]);
        const body = await jsonBody(request);
        record(body);
        digest(body.nodeSecretHash);
        const spec = await env.NODES.getByName(match[1]).enroll(await sha256(token), body.nodeSecretHash);
        await env.REGISTRY.getByName("owner").registerNode(spec);
        return json({ node: spec });
      }
      const p = await principal(request, env);
      const registry = env.REGISTRY.getByName("owner");
      requireThat(parts[0] === "v1", "Unknown API", 404);
      if (!p.admin && p.node.role === "native")
        requireThat(parts.length === 3 && parts[1] === "node" &&
          ["hello", "heartbeat", "identity"].includes(parts[2]),
          "Native node credentials cannot access project data or other fleet executors", 403);
      if (request.method === "POST" && url.pathname === "/v1/node-enrollments") {
        admin(p);
        const spec = validateNode(await jsonBody(request), false);
        requireThat(spec.role === "native", "This enrollment flow requires a native node");
        for (const project of spec.projects) await requireProject(env, project);
        const token = `${spec.id}.enroll.${crypto.randomUUID().replaceAll("-", "")}${crypto.randomUUID().replaceAll("-", "")}`;
        const expires = Date.now() + 10 * 60_000;
        await env.NODES.getByName(spec.id).prepareEnrollment(spec, await sha256(token), expires);
        return json({ token, expires, node: spec }, 201);
      }
      if (!p.admin && p.node.taskOnly === true)
        requireThat(
          (parts.length === 3 &&
            parts[1] === "node" &&
            parts[2] === "identity" &&
            request.method === "GET") ||
            (parts[1] === "projects" &&
              (parts[3] === "tasks" ||
                (parts[3] === "activity" &&
                  parts.length === 4 &&
                  request.method === "GET") ||
                (parts[3] === "assistant" &&
                  (parts.length === 4 || parts.length === 5)))),
          "Task-only credentials cannot operate fleet executors or storage",
          403,
        );
      if (parts.length === 2 && parts[1] === "projects") {
        admin(p);
        if (request.method === "GET")
          return json(
            await Promise.all(
              (await registry.list("project")).map((id) =>
                env.PROJECTS.getByName(id).inventory(),
              ),
            ),
          );
        if (request.method === "POST") {
          const body = await jsonBody(request);
          record(body);
          identifier(body.id);
          await env.PROJECTS.getByName(body.id).create(body.id);
          await registry.add("project", body.id);
          return json({ id: body.id }, 201);
        }
      }
      if (parts.length === 2 && parts[1] === "nodes") {
        admin(p);
        if (request.method === "GET")
          return json(
            await Promise.all(
              (await registry.list("node")).map((id) =>
                env.NODES.getByName(id).view(),
              ),
            ),
          );
        if (request.method === "POST") {
          const spec = validateNode(
            await jsonBody(request),
            String(env.ALLOW_HTTP_NODES) === "true",
          );
          for (const project of spec.projects)
            await requireProject(env, project);
          const secret =
            crypto.randomUUID().replaceAll("-", "") +
            crypto.randomUUID().replaceAll("-", "");
          const token = `${spec.id}.${secret}`;
          await env.NODES.getByName(spec.id).create(spec, await sha256(token));
          await registry.registerNode(spec);
          return json({ node: spec, token }, 201);
        }
      }
      if (
        parts[1] === "nodes" &&
        parts.length === 4 &&
        parts[3] === "revoke" &&
        request.method === "POST"
      ) {
        admin(p);
        identifier(parts[2]);
        await env.NODES.getByName(parts[2]).revoke();
        return json({ revoked: true });
      }
      if (
        parts[1] === "nodes" &&
        parts.length === 4 &&
        parts[3] === "reconcile" &&
        request.method === "POST"
      ) {
        admin(p);
        identifier(parts[2]);
        const body = await jsonBody(request);
        record(body);
        record(body.assignment);
        const a = body.assignment as unknown as Assignment;
        identifier(a.project);
        identifier(a.id);
        requireThat(
          a.node === parts[2] && body.stopped === true,
          "Owner must confirm the old executor is stopped",
          409,
        );
        await env.PROJECTS.getByName(a.project).interrupt(a);
        await env.NODES.getByName(parts[2]).retire(a);
        return json({ ok: true });
      }
      if (parts[1] === "node" && parts.length === 3) {
        const n = node(p);
        const stub = env.NODES.getByName(n.id);
        if (parts[2] === "hello" && request.method === "POST") {
          await registry.registerNode(n);
          const body = await jsonBody(request);
          record(body);
          requireThat(
            typeof body.session === "string" &&
              /^[a-f0-9-]{36}$/.test(body.session),
            "Invalid session",
          );
          return json(await stub.hello(body.session));
        }
        if (parts[2] === "identity" && request.method === "GET") {
          return json({
            id: n.id,
            role: n.role,
            projects: n.projects,
            taskOnly: n.taskOnly === true,
          });
        }
        if (parts[2] === "heartbeat" && request.method === "POST") {
          const s = session(request);
          const body = await jsonBody(request);
          record(body);
          record(body.stats);
          requireThat(
            Object.keys(body.stats).length <= 32 &&
              Object.values(body.stats).every(
                (v) => typeof v === "number" && Number.isFinite(v) && v >= 0,
              ),
            "Invalid statistics",
          );
          return json(
            await stub.heartbeat(
              s.epoch,
              s.session,
              body.stats as Record<string, number>,
            ),
          );
        }
        if (parts[2] === "reconcile" && request.method === "POST") {
          const s = session(request);
          const body = await jsonBody(request);
          record(body);
          requireThat(typeof body.attempt === "string", "Attempt required");
          const a = (await stub.reservations(s.epoch, s.session)).find(
            (r) => r.attempt === body.attempt,
          );
          requireThat(a, "Unknown attempt", 404);
          requireThat(
            readOnlyJob(a) && (a.epoch !== s.epoch || a.expires <= Date.now()),
            "Native or active work requires owner reconciliation",
            409,
          );
          await env.PROJECTS.getByName(a.project).interrupt(a);
          await stub.retire(a);
          return json({ ok: true });
        }
        if (parts[2] === "catalog" && request.method === "GET") {
          const s = session(request);
          await stub.reservations(s.epoch, s.session);
          return json(
            await Promise.all(
              n.projects.map(async (project) =>
                visibleInventory(
                  p,
                  await env.PROJECTS.getByName(project).inventory(),
                ),
              ),
            ),
          );
        }
        if (parts[2] === "ready" && request.method === "POST") {
          const s = session(request);
          const body = await jsonBody(request);
          record(body);
          identifier(body.project);
          digest(body.generation);
          requireThat(typeof body.ready === "boolean", "Ready flag required");
          projectAccess(p, body.project);
          requireThat(
            await env.PROJECTS.getByName(body.project).generation(
              body.generation,
            ),
            "Unknown generation",
            404,
          );
          await stub.ready(
            body.project,
            body.generation,
            s.epoch,
            s.session,
            body.ready,
          );
          return json({ ok: true });
        }
        if (parts[2] === "poll" && request.method === "POST") {
          const s = session(request);
          const reservations = await stub.reservations(s.epoch, s.session);
          if (
            reservations.some(
              (r) =>
                !readOnlyJob(r) &&
                (r.epoch !== s.epoch || r.expires <= Date.now()),
            )
          )
            return json({
              assignment: null,
              reconciliationRequired: true,
              reservations,
            });
          const own = reservations.filter(
            (r) =>
              r.epoch === s.epoch &&
              r.session === s.session &&
              r.expires > Date.now(),
          );
          if (own.length) {
            const a = own[0];
            await env.PROJECTS.getByName(a.project).assign(a);
            await stub.start(a);
            return json({ assignment: a });
          }
          if (reservations.length >= n.slots)
            return json({
              assignment: null,
              reconciliationRequired: true,
              reservations,
            });
          for (const project of n.projects) {
            const authority = env.PROJECTS.getByName(project);
            const next = await authority.next(
              n.assistantProjects?.includes(project) ?? false,
            );
            if (!next) continue;
            const reserved = await stub.reserve(next, s.epoch, s.session);
            try {
              await authority.assign(reserved);
            } catch (error) {
              if (
                (await authority.job(next.id))?.assignment?.attempt !==
                reserved.attempt
              )
                await stub.discard(project, next.id, reserved.attempt);
              throw error;
            }
            await stub.start(reserved);
            return json({ assignment: reserved });
          }
          return json({ assignment: null });
        }
        if (parts[2] === "renew" && request.method === "POST") {
          const { assignment: a } = await assignmentBody(request, p);
          const expires = await stub.renew(a);
          await env.PROJECTS.getByName(a.project).renew(a, expires);
          return json({ expires });
        }
        if (parts[2] === "finish" && request.method === "POST") {
          const { assignment: a, body } = await assignmentBody(request, p);
          requireThat(typeof body.ok === "boolean", "Result status required");
          requireThat(body.result !== undefined, "Result body required");
          const job = await env.PROJECTS.getByName(a.project).finish(
            a,
            canonical(body.result),
            body.ok,
          );
          await stub.finish(a, canonical(body.result));
          return json(job);
        }
      }
      if (parts[1] === "projects" && parts.length >= 3) {
        const project = part(parts, 2);
        projectAccess(p, project);
        await requireProject(env, project);
        const authority = env.PROJECTS.getByName(project);
        if (
          parts[3] === "assistant" &&
          (parts.length === 4 || parts.length === 5)
        ) {
          requireThat(
            p.admin || p.node.role === "agent",
            "Assistant access requires an agent or owner",
            403,
          );
          return await assistantRoute(request, env, project, parts[4], () =>
            jsonBody(request),
          );
        }
        if (parts[3] === "tasks") {
          requireThat(
            p.admin || p.node.role === "agent",
            "Task access requires an agent or owner",
            403,
          );
          const actor = p.admin
            ? { id: "owner", owner: true }
            : { id: `node:${p.node.id}`, owner: false };
          if (parts.length === 4) {
            if (request.method === "POST")
              return json(
                await authority.createTask(
                  validateTaskCreate(await jsonBody(request)),
                  actor,
                ),
                201,
              );
            if (request.method === "GET") {
              const cursor = url.searchParams.get("cursor") ?? "";
              if (cursor) identifier(cursor);
              const limit = Number(url.searchParams.get("limit") ?? 50);
              integer(limit, 1, 100);
              const status = url.searchParams.get("status") ?? undefined;
              if (status !== undefined) taskStatus(status);
              return json(await authority.listTasks(cursor, limit, status));
            }
          }
          if (
            parts.length === 5 &&
            parts[4] === "activity" &&
            request.method === "GET"
          ) {
            const cursor = Number(url.searchParams.get("cursor") ?? 0);
            integer(cursor, 0, Number.MAX_SAFE_INTEGER);
            const limit = Number(url.searchParams.get("limit") ?? 50);
            integer(limit, 1, 100);
            return json(await authority.taskActivity(cursor, limit));
          }
          identifier(parts[4]);
          if (parts.length === 5 && request.method === "GET") {
            const task = await authority.task(parts[4]);
            requireThat(task, "Unknown task", 404);
            return json(task);
          }
          if (parts.length === 6 && request.method === "POST") {
            if (parts[5] === "update")
              return json(
                await authority.updateTask(
                  parts[4],
                  validateTaskUpdate(await jsonBody(request)),
                  actor,
                ),
              );
            if (parts[5] === "move")
              return json(
                await authority.moveTask(
                  parts[4],
                  validateTaskMove(await jsonBody(request)),
                  actor,
                ),
              );
          }
        }
        if (
          parts.length === 4 &&
          parts[3] === "activity" &&
          request.method === "GET"
        ) {
          requireThat(
            p.admin || p.node.role === "agent",
            "Activity requires an agent or owner",
            403,
          );
          const inventory = await authority.inventory();
          const views = await mapLimit(await registry.list("node"), 16, (id) =>
            env.NODES.getByName(id).view(),
          );
          return json({
            jobs: inventory.jobs.map((job) => ({
              id: job.spec.id,
              status: job.status,
              generation: job.spec.generation,
              kind: job.spec.kind,
              ...(job.spec.check ? { check: job.spec.check } : {}),
              ...(job.assignment
                ? {
                    assignment: {
                      node: job.assignment.node,
                      attempt: job.assignment.attempt,
                      expires: job.assignment.expires,
                    },
                  }
                : {}),
            })),
            nodes: views
              .filter(
                (v) =>
                  v &&
                  !v.revoked &&
                  !v.taskOnly &&
                  v.projects.includes(project),
              )
              .map((v) => ({
                id: v!.id,
                role: v!.role,
                slots: v!.slots,
                lastSeen: v!.lastSeen,
                online: v!.lastSeen > Date.now() - NODE_STALE_MS,
                stats: v!.stats,
              })),
          });
        }
        if (parts.length === 3 && request.method === "GET")
          return json(visibleInventory(p, await authority.inventory()));
        if (
          parts.length === 4 &&
          parts[3] === "checks" &&
          request.method === "POST"
        ) {
          admin(p);
          const body = await jsonBody(request);
          record(body);
          return json(
            await authority.configureChecks(validateChecks(body.checks)),
          );
        }
        if (
          parts.length === 4 &&
          parts[3] === "candidates" &&
          request.method === "POST"
        ) {
          admin(p);
          const spec = validateCandidate(await jsonBody(request));
          requireThat(spec.project === project, "Project mismatch");
          return json(await authority.stage(spec), 201);
        }
        if (
          parts.length === 5 &&
          parts[3] === "candidates" &&
          request.method === "GET"
        ) {
          identifier(parts[4]);
          const candidate = await authority.candidate(parts[4]);
          requireThat(candidate, "Unknown candidate", 404);
          return json(candidate);
        }
        if (
          parts.length === 4 &&
          parts[3] === "jobs" &&
          request.method === "POST"
        ) {
          admin(p);
          const spec = validateJob(await jsonBody(request));
          requireThat(spec.project === project, "Project mismatch");
          return json(await authority.enqueue(spec), 201);
        }
        if (
          parts.length === 5 &&
          parts[3] === "jobs" &&
          request.method === "GET"
        ) {
          requireThat(
            p.admin || p.node.role === "agent",
            "Job access requires an agent or owner",
            403,
          );
          identifier(parts[4]);
          const job = await authority.job(parts[4]);
          requireThat(job, "Unknown job", 404);
          return json(job);
        }
        if (
          parts.length === 4 &&
          parts[3] === "accept" &&
          request.method === "POST"
        ) {
          admin(p);
          const body = await jsonBody(request);
          record(body);
          digest(body.generation);
          if (body.expected !== null) digest(body.expected);
          if (body.candidate !== undefined) identifier(body.candidate);
          return json(
            await authority.accept(
              body.generation,
              body.expected,
              body.candidate as string | undefined,
            ),
          );
        }
        if (
          parts.length === 4 &&
          parts[3] === "generations" &&
          request.method === "POST"
        ) {
          requireThat(
            p.admin || p.node.role === "agent",
            "Only agent nodes publish generations",
            403,
          );
          const manifest = validateGeneration(await jsonBody(request));
          requireThat(manifest.project === project, "Project mismatch");
          await mapLimit(manifest.pages, 16, async (page) => {
            const object = await env.STORE.head(
              `projects/${project}/pages/${page.hash}`,
            );
            requireThat(
              object?.size === page.bytes &&
                object.customMetadata?.count === String(page.count) &&
                object.customMetadata?.contentBytes ===
                  String(page.contentBytes),
              "Index page is not durably verified",
              409,
            );
          });
          const text = canonical(manifest);
          const hash = await sha256(text);
          await immutablePut(
            env,
            `projects/${project}/generations/${hash}`,
            text,
            { sha256: hash },
          );
          return json(await authority.publish(hash, manifest), 201);
        }
        if (
          parts.length === 5 &&
          parts[3] === "generations" &&
          request.method === "GET"
        ) {
          digest(parts[4]);
          const generation = await authority.generation(parts[4]);
          requireThat(generation, "Unknown generation", 404);
          return json(generation);
        }
        if (
          parts.length === 5 &&
          parts[3] === "routes" &&
          request.method === "GET"
        ) {
          digest(parts[4]);
          requireThat(
            await authority.generation(parts[4]),
            "Unknown generation",
            404,
          );
          const views = await mapLimit(
            await registry.storage(project),
            16,
            (id) => env.NODES.getByName(id).view(),
          );
          const storage = views
            .filter((v) => v !== null)
            .filter(
              (v) =>
                v.role === "storage" &&
                !v.revoked &&
                v.projects.includes(project) &&
                v.origin &&
                v.lastSeen > Date.now() - NODE_STALE_MS,
            );
          storage.sort(
            (a, b) =>
              Number((b.ready[project] ?? []).includes(parts[4])) -
                Number((a.ready[project] ?? []).includes(parts[4])) ||
              a.id.localeCompare(b.id),
          );
          return json({
            generation: parts[4],
            expires: Date.now() + 60_000,
            origins: await Promise.all(
              storage.slice(0, 4).map(async (v) => ({
                node: v.id,
                origin: v.origin,
                ready: (v.ready[project] ?? []).includes(parts[4]),
                token: await env.NODES.getByName(v.id).grant(
                  project,
                  parts[4],
                  p.admin ? "owner" : p.node.id,
                ),
              })),
            ),
          });
        }
      }
      if (parts[1] === "data" && parts.length === 5) {
        const project = parts[2],
          kind = parts[3],
          hash = parts[4];
        projectAccess(p, project);
        digest(hash);
        requireThat(
          kind === "chunks" || kind === "pages",
          "Unknown data kind",
          404,
        );
        await requireProject(env, project);
        const key = `projects/${project}/${kind}/${hash}`;
        if (request.method === "HEAD") {
          const object = await env.STORE.head(key);
          return new Response(null, {
            status: object ? 200 : 404,
            headers: object ? { "content-length": String(object.size) } : {},
          });
        }
        if (request.method === "GET") {
          const object = await env.STORE.get(key);
          requireThat(object, "Object not found", 404);
          return new Response(object.body, {
            headers: {
              "content-type":
                kind === "pages"
                  ? "application/json"
                  : "application/octet-stream",
              "content-length": String(object.size),
              etag: hash,
              "cache-control": "private, immutable",
              "x-content-type-options": "nosniff",
            },
          });
        }
        if (request.method === "PUT") {
          requireThat(
            p.admin || p.node.role === "agent",
            "Storage nodes cannot publish project objects",
            403,
          );
          const bytes = await readBounded(
            request.body,
            kind === "chunks" ? MAX_CHUNK_BYTES : MAX_JSON_BYTES,
          );
          requireThat(
            bytes.byteLength > 0 && (await sha256(bytes)) === hash,
            "Object digest mismatch",
          );
          let metadata: Record<string, string> = { sha256: hash };
          if (kind === "pages") {
            const chunks = validateChunks(
              JSON.parse(new TextDecoder().decode(bytes)),
            );
            await mapLimit(chunks, 16, async (chunk) => {
              const object = await env.STORE.head(
                `projects/${project}/chunks/${chunk.hash}`,
              );
              requireThat(
                object?.size === chunk.bytes &&
                  object.customMetadata?.sha256 === chunk.hash,
                "Missing or unverified chunk",
                409,
              );
            });
            metadata = {
              ...metadata,
              count: String(chunks.length),
              contentBytes: String(chunks.reduce((n, c) => n + c.bytes, 0)),
            };
          }
          await immutablePut(env, key, bytes, metadata);
          return json({ hash, bytes: bytes.byteLength }, 201);
        }
      }
      throw new ProtocolError(404, "Unknown route");
    } catch (error) {
      const message = error instanceof Error ? error.message : "Internal error";
      const match = message.match(/\[EBOX (\d{3})\] (.*)/s);
      const status =
        error instanceof ProtocolError
          ? error.status
          : match
            ? Number(match[1])
            : error instanceof SyntaxError
              ? 400
              : 500;
      if (status === 500)
        console.error(JSON.stringify({ event: "request_failed", message }));
      return json(
        {
          error:
            status === 500
              ? "Internal controller error"
              : (match?.[2] ?? message),
        },
        status,
      );
    }
  },
} satisfies ExportedHandler<Env>;
