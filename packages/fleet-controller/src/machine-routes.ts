import {
  digest,
  exactKeys,
  identifier,
  integer,
  jsonBody,
  MACHINE_INVITATION_MS,
  MAX_CHUNK_BYTES,
  NODE_STALE_MS,
  record,
  requireThat,
  sha256,
  validateMachineGrant,
  validateMachineInvitation,
  validateMachineOpen,
  validateMachineSignal,
  machineGrantToken,
  type NodeView,
  type NodeSpec,
} from "@enginebox/fleet-protocol";
import { machineIce } from "./machine-turn.ts";
import type { MachineAuthority } from "./machines.ts";
export type MachinePrincipal =
  | { admin: true }
  | { admin: false; node: NodeView };
const json = (value: unknown, status = 200) =>
  Response.json(value, {
    status,
    headers: {
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
    },
  });
const random = () =>
  crypto.randomUUID().replaceAll("-", "") +
  crypto.randomUUID().replaceAll("-", "");
export function machineEpoch(request: Request) {
  const epoch = Number(request.headers.get("x-enginebox-epoch"));
  integer(epoch, 1, Number.MAX_SAFE_INTEGER);
  const session = request.headers.get("x-enginebox-session");
  requireThat(
    typeof session === "string" && /^[a-f0-9-]{36}$/.test(session),
    "Node session required",
  );
  return { epoch, session };
}
async function machine(env: Env, project: string, id: string) {
  identifier(id);
  const node = await env.NODES.getByName(id).machineView();
  requireThat(
    node &&
      node.role === "native" &&
      node.projects.length === 1 &&
      node.projects[0] === project,
    "Unknown project machine",
    404,
  );
  return node;
}
export async function syncNative(env: Env, id: string) {
  const node = await env.NODES.getByName(id).machineView();
  if (node?.role === "native")
    await env.PROJECTS.getByName(node.projects[0]).syncMachine(
      id,
      node.epoch,
      node.revoked,
    );
}
export async function nativeEnrollment(request: Request, env: Env) {
  const token =
    request.headers.get("authorization")?.match(/^Bearer ([^\s]+)$/)?.[1] ?? "";
  const match = /^([a-z0-9][a-z0-9-]{0,47})\.enroll\.[a-f0-9]{64}$/.exec(token);
  requireThat(match, "Machine invitation required", 401);
  const body = await jsonBody(request);
  record(body);
  exactKeys(body, ["nodeSecretHash"]);
  digest(body.nodeSecretHash);
  const spec = await env.NODES.getByName(match[1]).enroll(
    await sha256(token),
    body.nodeSecretHash,
  );
  await env.REGISTRY.getByName("owner").registerNode(spec);
  await syncNative(env, spec.id);
  return json({ node: spec });
}
export async function nativeMachinePoll(
  request: Request,
  env: Env,
  p: MachinePrincipal,
) {
  requireThat(
    !p.admin && p.node.role === "native",
    "Native machine required",
    403,
  );
  const body = await jsonBody(request);
  record(body);
  exactKeys(body, []);
  const active = machineEpoch(request);
  await env.NODES.getByName(p.node.id).checkSession(
    active.epoch,
    active.session,
  );
  return json(
    await env.PROJECTS.getByName(p.node.projects[0]).machinePoll(
      p.node.id,
      active.epoch,
    ),
  );
}
async function sessionAuthority(
  request: Request,
  env: Env,
  p: MachinePrincipal,
): Promise<MachineAuthority> {
  if (p.admin) return { kind: "owner" };
  if (p.node.role === "native") {
    const active = machineEpoch(request);
    // current() is synchronous internal; heartbeat checks persisted fencing via RPC.
    await env.NODES.getByName(p.node.id).checkSession(
      active.epoch,
      active.session,
    );
    return { kind: "host", nodeId: p.node.id, epoch: active.epoch };
  }
  requireThat(
    p.node.taskOnly === true,
    "Project task credential required",
    403,
  );
  const secret = request.headers.get("x-enginebox-stream-client");
  requireThat(
    typeof secret === "string" && /^[a-f0-9]{64}$/.test(secret),
    "Stream client secret required",
    403,
  );
  return { kind: "client", hash: await sha256(secret) };
}
export async function machineRoute(
  request: Request,
  env: Env,
  project: string,
  parts: string[],
  p: MachinePrincipal,
) {
  const authority = env.PROJECTS.getByName(project),
    kind = parts[3];
  if (
    kind === "machine-invitations" &&
    parts.length === 4 &&
    request.method === "POST"
  ) {
    requireThat(p.admin, "Owner authorization required", 403);
    const input = validateMachineInvitation(await jsonBody(request));
    const registry = env.REGISTRY.getByName("owner");
    const all = await registry.machines(project);
    requireThat(
      all.includes(input.id) || all.length < 256,
      "Project machine capacity reached",
      429,
    );
    const spec: NodeSpec = {
      id: input.id,
      role: "native",
      projects: [project],
      origin: null,
      slots: 1,
      cacheBytes: MAX_CHUNK_BYTES * 2,
      native: input.native,
    };
    const token = `${input.id}.enroll.${random()}`;
    const result = await env.NODES.getByName(input.id).invite(
      spec,
      token,
      await sha256(token),
      Date.now() + MACHINE_INVITATION_MS,
    );
    await registry.registerMachine(project, input.id);
    return json(result, 201);
  }
  if (kind === "machines" && parts.length === 4 && request.method === "GET") {
    requireThat(
      p.admin || (p.node.role === "agent" && p.node.taskOnly),
      "Project task credential required",
      403,
    );
    const ids = await env.REGISTRY.getByName("owner").machines(project);
    const nodes = await Promise.all(ids.map((id) => machine(env, project, id)));
    return json({
      machines: nodes.map((node) => ({
        id: node.id,
        ...node.native,
        online:
          !node.revoked &&
          node.epoch > 0 &&
          Date.now() - node.lastSeen < NODE_STALE_MS,
        epoch: node.epoch,
        lastSeen: node.lastSeen,
        stats: node.stats,
        revoked: node.revoked,
        ...("pending" in node
          ? { pending: node.pending, invitationExpires: node.invitationExpires }
          : { pending: false }),
      })),
    });
  }
  if (
    kind === "machines" &&
    parts.length === 6 &&
    parts[5] === "revoke" &&
    request.method === "POST"
  ) {
    requireThat(p.admin, "Owner authorization required", 403);
    const body = await jsonBody(request);
    record(body);
    exactKeys(body, ["operationId"]);
    identifier(body.operationId);
    await machine(env, project, parts[4]);
    await env.NODES.getByName(parts[4]).revoke();
    await syncNative(env, parts[4]);
    return json({ revoked: true });
  }
  if (
    kind === "machine-session-grants" &&
    parts.length === 4 &&
    request.method === "POST"
  ) {
    requireThat(p.admin, "Owner authorization required", 403);
    const input = validateMachineGrant(await jsonBody(request));
    const node = await machine(env, project, input.nodeId);
    requireThat(
      !node.revoked &&
        node.epoch > 0 &&
        Date.now() - node.lastSeen < NODE_STALE_MS,
      "Machine is offline",
      409,
    );
    requireThat(
      node.stats.webrtc === 1 && node.stats.video === 1,
      "Machine media runtime is unavailable",
      409,
    );
    requireThat(
      input.mode !== "control" ||
        (node.native?.allowControl === true && node.stats.input === 1),
      "Machine control is disabled",
      403,
    );
    await syncNative(env, node.id);
    const prior = await authority.machineGrantExisting(input, node.epoch);
    if (prior) return json(prior);
    const ice =
        (await authority.machineGrantIce(input, node.epoch)) ??
        (await machineIce(env)),
      token = `${input.id}.stream.${random()}`;
    return json(
      await authority.machineGrant(
        input,
        node.epoch,
        token,
        await sha256(token),
        ice,
      ),
      201,
    );
  }
  if (kind === "machine-sessions") {
    if (parts.length === 4 && request.method === "POST") {
      requireThat(
        !p.admin && p.node.role === "agent" && p.node.taskOnly === true,
        "Project task credential required",
        403,
      );
      const input = validateMachineOpen(await jsonBody(request));
      return json(
        await authority.machineOpen(
          input.id,
          input.grant.split(".")[0],
          await sha256(input.grant),
          input.clientSecretHash,
        ),
        201,
      );
    }
    identifier(parts[4]);
    const auth = await sessionAuthority(request, env, p),
      id = parts[4];
    if (parts.length === 5 && request.method === "GET")
      return json(await authority.machineSession(id, auth));
    if (parts.length === 6 && parts[5] === "signals") {
      requireThat(
        auth.kind !== "owner",
        "Owner cannot access stream signaling",
        403,
      );
      if (request.method === "GET") {
        const after = Number(
          new URL(request.url).searchParams.get("after") ?? "0",
        );
        integer(after, 0, 258);
        return json(await authority.machineSignals(id, auth, after));
      }
      if (request.method === "POST")
        return json(
          await authority.machineSignal(
            id,
            auth,
            validateMachineSignal(await jsonBody(request)),
          ),
        );
    }
    if (
      parts.length === 6 &&
      parts[5] === "renew" &&
      request.method === "POST"
    ) {
      const body = await jsonBody(request);
      record(body);
      exactKeys(body, ["grant"]);
      machineGrantToken(body.grant);
      return json(
        await authority.machineRenew(
          id,
          auth,
          body.grant.split(".")[0],
          await sha256(body.grant),
        ),
      );
    }
    if (parts.length === 6 && parts[5] === "end" && request.method === "POST") {
      const body = await jsonBody(request);
      record(body);
      exactKeys(body, ["id"]);
      identifier(body.id);
      return json(await authority.machineEnd(id, auth, body.id));
    }
  }
  requireThat(false, "Unknown machine route", 404);
}
