import {
  identifier,
  NODE_STALE_MS,
  requireThat,
  validateJob,
} from "@enginebox/fleet-protocol";
const json = (value: unknown, init?: ResponseInit) =>
  Response.json(value, {
    ...init,
    headers: {
      "cache-control": "no-store",
      "x-content-type-options": "nosniff",
    },
  });

export async function assistantRoute(
  request: Request,
  env: Env,
  project: string,
  id: string | undefined,
  body: () => Promise<unknown>,
) {
  const authority = env.PROJECTS.getByName(project);
  async function available() {
    const ids = await env.REGISTRY.getByName("owner").assistants(project);
    for (let offset = 0; offset < ids.length; offset += 16) {
      const nodes = await Promise.all(
        ids.slice(offset, offset + 16).map((id) =>
          env.NODES.getByName(id)
            .view()
            .catch(() => null),
        ),
      );
      if (
        nodes.some(
          (node) =>
            node &&
            !node.revoked &&
            node.role === "agent" &&
            !(node.stats.uncertainNativeJobs > 0) &&
            node.lastSeen > Date.now() - NODE_STALE_MS &&
            node.assistantProjects?.includes(project),
        )
      )
        return true;
    }
    return false;
  }
  if (!id && request.method === "GET") {
    const ready = await available();
    return json({
      available: ready,
      provider: "codex",
      ...(!ready
        ? {
            reason:
              "No live node is configured with a native Codex workspace and project task tools",
          }
        : {}),
    });
  }
  if (!id && request.method === "POST") {
    const value = (await body()) as Record<string, unknown>;
    requireThat(
      value !== null && typeof value === "object" && !Array.isArray(value),
      "Expected an object",
    );
    requireThat(
      Object.keys(value).every((key) => ["id", "prompt"].includes(key)),
      "Unknown assistant field",
    );
    const spec = validateJob({
      id: value.id,
      project,
      kind: "assistant",
      generation: null,
      prompt: value.prompt,
      timeoutSeconds: 300,
    });
    const prior = await authority.job(spec.id);
    requireThat(
      prior || (await available()),
      "No configured native assistant node is available",
      503,
    );
    const job = await authority.enqueue(spec);
    return json({ id: job.spec.id, status: job.status }, { status: 202 });
  }
  if (id && request.method === "GET") {
    identifier(id);
    const job = await authority.job(id);
    requireThat(job?.spec.kind === "assistant", "Unknown assistant run", 404);
    const result = job.result ? JSON.parse(job.result) : null;
    const executor =
      job.status === "assigned" && job.assignment
        ? await env.NODES.getByName(job.assignment.node).view()
        : null;
    const blocked = (executor?.stats.uncertainNativeJobs ?? 0) > 0;
    return json({
      id,
      status: job.status,
      ...(blocked
        ? {
            blocked: true,
            error: "Native executor outcome needs owner reconciliation",
          }
        : {}),
      ...(job.status === "completed"
        ? {
            text: String(
              result?.text ?? "Codex completed without a text response.",
            ),
          }
        : {}),
      ...(job.status === "failed"
        ? {
            error: String(result?.error ?? "Native assistant run failed").slice(
              0,
              500,
            ),
          }
        : {}),
    });
  }
  return json({ error: "Unknown assistant route" }, { status: 404 });
}
