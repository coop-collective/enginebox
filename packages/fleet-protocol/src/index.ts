import { validateIntegration, type IntegrationJob } from "./integration.ts";
export * from "./integration.ts";
export const SCHEMA = 1;
export const CHUNK_BYTES = 4 * 1024 * 1024;
export const MAX_CHUNK_BYTES = 8 * 1024 * 1024;
export const PAGE_CHUNKS = 256;
export const MAX_PAGES = 256;
export const MAX_JSON_BYTES = 256 * 1024;
export const LEASE_MS = 30_000;
export const NODE_STALE_MS = 20_000;
export type Role = "agent" | "storage";
export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };
export type Chunk = { hash: string; bytes: number };
export type IndexPage = {
  hash: string;
  bytes: number;
  count: number;
  contentBytes: number;
};
export type Generation = {
  schema: 1;
  project: string;
  parent: string | null;
  format: "sqlite";
  snapshotHash: string;
  snapshotBytes: number;
  pages: IndexPage[];
  metadata: {
    layerfsRevision?: string;
    projectName?: string;
    [key: string]: string | undefined;
  };
};
export type NodeSpec = {
  id: string;
  role: Role;
  projects: string[];
  origin: string | null;
  slots: number;
  cacheBytes: number;
  assistantProjects?: string[];
  integrationProjects?: string[];
  taskOnly?: true;
};
export type NodeView = NodeSpec & {
  epoch: number;
  session: string | null;
  lastSeen: number;
  revoked: boolean;
  ready: Record<string, string[]>;
  stats: Record<string, number>;
};
export type StoreJob = {
  id: string;
  project: string;
  generation: string;
  kind: "hydrate" | "verify" | "layerfs-exec";
  argv?: string[];
  timeoutSeconds?: number;
  check?: {
    candidate: string;
    name: string;
    recipe: string;
    scope: "full" | "selected";
  };
};
export type AssistantJob = {
  id: string;
  project: string;
  kind: "assistant";
  generation: null;
  prompt: string;
  timeoutSeconds: number;
  argv?: never;
  check?: never;
};
export type JobSpec = StoreJob | AssistantJob | IntegrationJob;
export function readOnlyJob(job: { kind: string }) {
  return job.kind === "hydrate" || job.kind === "verify";
}
export type CheckRequirement = {
  name: string;
  recipe: string;
  kind: "verify" | "layerfs-exec";
  argv?: string[];
  timeoutSeconds?: number;
};
export type CandidateSpec = {
  id: string;
  project: string;
  base: string | null;
  generation: string;
  changes: { id: string; generation: string }[];
};
export type Assignment = JobSpec & {
  node: string;
  epoch: number;
  session: string;
  expires: number;
  attempt: string;
};

export class ProtocolError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(`[EBOX ${status}] ${message}`);
    this.status = status;
  }
}
export function requireThat(
  value: unknown,
  message: string,
  status = 400,
): asserts value {
  if (!value) throw new ProtocolError(status, message);
}
export function identifier(value: unknown): asserts value is string {
  requireThat(
    typeof value === "string" &&
      /^[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?$/.test(value),
    "Invalid identifier",
  );
}
export function digest(value: unknown): asserts value is string {
  requireThat(
    typeof value === "string" && /^[a-f0-9]{64}$/.test(value),
    "Invalid SHA-256 digest",
  );
}
export function integer(
  value: unknown,
  min: number,
  max: number,
): asserts value is number {
  requireThat(
    Number.isSafeInteger(value) && Number(value) >= min && Number(value) <= max,
    "Integer outside allowed range",
  );
}
export function record(
  value: unknown,
): asserts value is Record<string, unknown> {
  requireThat(
    value !== null && typeof value === "object" && !Array.isArray(value),
    "Expected an object",
  );
}
export function exactKeys(value: Record<string, unknown>, allowed: string[]) {
  requireThat(
    Object.keys(value).every((key) => allowed.includes(key)),
    "Unknown field",
  );
}
export function validateChunks(value: unknown): Chunk[] {
  requireThat(
    Array.isArray(value) && value.length > 0 && value.length <= PAGE_CHUNKS,
    "Invalid index page",
  );
  return value.map((item) => {
    record(item);
    exactKeys(item, ["hash", "bytes"]);
    digest(item.hash);
    integer(item.bytes, 1, MAX_CHUNK_BYTES);
    return { hash: item.hash, bytes: item.bytes };
  });
}
export function validateGeneration(value: unknown): Generation {
  record(value);
  exactKeys(value, [
    "schema",
    "project",
    "parent",
    "format",
    "snapshotHash",
    "snapshotBytes",
    "pages",
    "metadata",
  ]);
  requireThat(
    value.schema === SCHEMA && value.format === "sqlite",
    "Unsupported generation format",
  );
  identifier(value.project);
  if (value.parent !== null) digest(value.parent);
  digest(value.snapshotHash);
  integer(value.snapshotBytes, 1, MAX_CHUNK_BYTES * PAGE_CHUNKS * MAX_PAGES);
  requireThat(
    Array.isArray(value.pages) &&
      value.pages.length > 0 &&
      value.pages.length <= MAX_PAGES,
    "Invalid index pages",
  );
  const pages = value.pages.map((page) => {
    record(page);
    exactKeys(page, ["hash", "bytes", "count", "contentBytes"]);
    digest(page.hash);
    integer(page.bytes, 1, MAX_JSON_BYTES);
    integer(page.count, 1, PAGE_CHUNKS);
    integer(page.contentBytes, 1, MAX_CHUNK_BYTES * PAGE_CHUNKS);
    return {
      hash: page.hash,
      bytes: page.bytes,
      count: page.count,
      contentBytes: page.contentBytes,
    };
  });
  requireThat(
    pages.reduce((n, p) => n + p.contentBytes, 0) === value.snapshotBytes,
    "Snapshot/index byte counts differ",
  );
  record(value.metadata);
  requireThat(
    Object.keys(value.metadata).length <= 16 &&
      Object.entries(value.metadata).every(
        ([k, v]) =>
          /^[a-zA-Z][a-zA-Z0-9]*$/.test(k) &&
          typeof v === "string" &&
          v.length <= 256,
      ),
    "Invalid generation metadata",
  );
  return {
    schema: 1,
    project: value.project,
    parent: value.parent,
    format: "sqlite",
    snapshotHash: value.snapshotHash,
    snapshotBytes: value.snapshotBytes,
    pages,
    metadata: value.metadata as Generation["metadata"],
  };
}
export function validateNode(value: unknown, allowHttp: boolean): NodeSpec {
  record(value);
  exactKeys(value, [
    "id",
    "role",
    "projects",
    "origin",
    "slots",
    "cacheBytes",
    "assistantProjects",
    "integrationProjects",
    "taskOnly",
  ]);
  identifier(value.id);
  requireThat(
    value.role === "agent" || value.role === "storage",
    "Invalid node role",
  );
  requireThat(
    Array.isArray(value.projects) &&
      value.projects.length > 0 &&
      value.projects.length <= 128,
    "Projects required",
  );
  for (const project of value.projects) identifier(project);
  if (value.taskOnly !== undefined)
    requireThat(
      value.taskOnly === true && value.role === "agent" &&
        value.projects.length === 1 && value.origin === null &&
        value.assistantProjects === undefined && value.integrationProjects === undefined,
      "Task-only credentials require one project, agent role and no executor capability",
    );
  if (value.assistantProjects !== undefined) {
    requireThat(
      value.role === "agent" &&
        Array.isArray(value.assistantProjects) &&
        value.assistantProjects.length <= 128,
      "Invalid assistant projects",
    );
    for (const project of value.assistantProjects) {
      identifier(project);
      requireThat(
        value.projects.includes(project),
        "Assistant project must be enrolled",
      );
    }
  }
  if (value.integrationProjects !== undefined) {
    requireThat(value.role === "agent" && Array.isArray(value.integrationProjects) && value.integrationProjects.length <= 128, "Invalid integration projects");
    for (const project of value.integrationProjects) { identifier(project); requireThat(value.projects.includes(project), "Integration project must be enrolled"); }
  }
  integer(value.slots, 1, 64);
  integer(value.cacheBytes, MAX_CHUNK_BYTES * 2, Number.MAX_SAFE_INTEGER);
  let origin: string | null = null;
  if (value.origin !== null) {
    requireThat(typeof value.origin === "string", "Invalid origin");
    let url: URL;
    try {
      url = new URL(value.origin);
    } catch {
      throw new ProtocolError(400, "Invalid storage origin");
    }
    requireThat(
      (url.protocol === "https:" || (allowHttp && url.protocol === "http:")) &&
        !url.username &&
        !url.password &&
        url.pathname === "/" &&
        !url.search &&
        !url.hash,
      "Invalid storage origin",
    );
    origin = url.origin;
  }
  requireThat(
    value.role !== "storage" || origin !== null,
    "Storage nodes require an origin",
  );
  return {
    id: value.id,
    role: value.role,
    projects: [...new Set(value.projects as string[])],
    origin,
    slots: value.slots,
    cacheBytes: value.cacheBytes,
    ...(value.taskOnly === true ? { taskOnly: true as const } : {}),
    ...(value.integrationProjects === undefined ? {} : { integrationProjects: [...new Set(value.integrationProjects as string[])] }),
    ...(value.assistantProjects === undefined
      ? {}
      : {
          assistantProjects: [...new Set(value.assistantProjects as string[])],
        }),
  };
}
export function validateJob(value: unknown): JobSpec {
  record(value);
  exactKeys(value, [
    "id",
    "project",
    "generation",
    "kind",
    "argv",
    "timeoutSeconds",
    "check",
    "prompt",
    "integration",
  ]);
  identifier(value.id);
  identifier(value.project);
  if (value.kind === "integration") {
    digest(value.generation);
    requireThat(value.argv === undefined && value.check === undefined && value.prompt === undefined && value.timeoutSeconds === undefined, "Integration cannot carry arbitrary commands or CI evidence");
    return { id: value.id, project: value.project, generation: value.generation, kind: "integration", integration: validateIntegration(value.integration) };
  }
  requireThat(value.integration === undefined, "Only integration jobs carry a plan");
  if (value.kind === "assistant") {
    requireThat(
      value.generation === null &&
        value.argv === undefined &&
        value.check === undefined,
      "Assistant jobs cannot claim Store or CI evidence",
    );
    requireThat(
      typeof value.prompt === "string" &&
        value.prompt.trim().length > 0 &&
        new TextEncoder().encode(value.prompt).byteLength <= 8000 &&
        !value.prompt.includes("\0"),
      "Prompt must contain 1–8000 UTF-8 bytes without NUL",
    );
    integer(value.timeoutSeconds, 1, 300);
    return {
      id: value.id,
      project: value.project,
      kind: "assistant",
      generation: null,
      prompt: value.prompt,
      timeoutSeconds: value.timeoutSeconds,
    };
  }
  requireThat(value.prompt === undefined, "Only assistant jobs accept prompts");
  digest(value.generation);
  requireThat(
    ["hydrate", "verify", "layerfs-exec"].includes(String(value.kind)),
    "Unknown job kind",
  );
  const job: StoreJob = {
    id: value.id,
    project: value.project,
    generation: value.generation,
    kind: value.kind as StoreJob["kind"],
  };
  if (value.check !== undefined) {
    record(value.check);
    exactKeys(value.check, ["candidate", "name", "recipe", "scope"]);
    identifier(value.check.candidate);
    identifier(value.check.name);
    digest(value.check.recipe);
    requireThat(
      value.check.scope === "full" || value.check.scope === "selected",
      "Invalid check scope",
    );
    job.check = {
      candidate: value.check.candidate,
      name: value.check.name,
      recipe: value.check.recipe,
      scope: value.check.scope,
    };
  }
  if (value.kind === "layerfs-exec") {
    requireThat(
      Array.isArray(value.argv) &&
        value.argv.length > 0 &&
        value.argv.length <= 64 &&
        value.argv.every(
          (arg) =>
            typeof arg === "string" &&
            arg.length <= 8192 &&
            !arg.includes("\0"),
        ),
      "Invalid command arguments",
    );
    integer(value.timeoutSeconds, 1, 3600);
    job.argv = value.argv as string[];
    job.timeoutSeconds = value.timeoutSeconds;
  } else
    requireThat(
      value.argv === undefined && value.timeoutSeconds === undefined,
      "Command parameters are only valid for layerfs-exec",
    );
  return job;
}
export function validateChecks(value: unknown): CheckRequirement[] {
  requireThat(
    Array.isArray(value) && value.length <= 16,
    "Invalid required checks",
  );
  const checks = value.map((v) => {
    record(v);
    exactKeys(v, ["name", "recipe", "kind", "argv", "timeoutSeconds"]);
    identifier(v.name);
    digest(v.recipe);
    requireThat(
      v.kind === "verify" || v.kind === "layerfs-exec",
      "Invalid check executor",
    );
    const executor = validateJob({
      id: "recipe",
      project: "recipe",
      generation: "0".repeat(64),
      kind: v.kind,
      ...(v.argv !== undefined ? { argv: v.argv } : {}),
      ...(v.timeoutSeconds !== undefined
        ? { timeoutSeconds: v.timeoutSeconds }
        : {}),
    });
    return {
      name: v.name,
      recipe: v.recipe,
      kind: executor.kind,
      ...(executor.argv
        ? { argv: executor.argv, timeoutSeconds: executor.timeoutSeconds }
        : {}),
    } as CheckRequirement;
  });
  requireThat(
    new Set(checks.map((c) => c.name)).size === checks.length,
    "Duplicate check name",
  );
  return checks.sort((a, b) => (a.name < b.name ? -1 : 1));
}
export function validateCandidate(value: unknown): CandidateSpec {
  record(value);
  exactKeys(value, ["id", "project", "base", "generation", "changes"]);
  identifier(value.id);
  identifier(value.project);
  digest(value.generation);
  if (value.base !== null) digest(value.base);
  requireThat(
    Array.isArray(value.changes) &&
      value.changes.length > 0 &&
      value.changes.length <= 128,
    "A candidate needs 1–128 changes",
  );
  for (const change of value.changes) {
    record(change);
    exactKeys(change, ["id", "generation"]);
    identifier(change.id);
    digest(change.generation);
  }
  requireThat(
    new Set(value.changes.map((c) => c.id)).size === value.changes.length,
    "Duplicate candidate change",
  );
  return {
    id: value.id,
    project: value.project,
    base: value.base,
    generation: value.generation,
    changes: value.changes as CandidateSpec["changes"],
  };
}
export function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value !== null && typeof value === "object")
    return `{${Object.entries(value)
      .filter(([, v]) => v !== undefined)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`)
      .join(",")}}`;
  return JSON.stringify(value);
}
export async function sha256(bytes: Uint8Array | string): Promise<string> {
  const data =
    typeof bytes === "string" ? new TextEncoder().encode(bytes) : bytes;
  return Array.from(
    new Uint8Array(await crypto.subtle.digest("SHA-256", data as BufferSource)),
    (b) => b.toString(16).padStart(2, "0"),
  ).join("");
}
export async function readBounded(
  body: ReadableStream<Uint8Array> | null,
  limit: number,
): Promise<Uint8Array> {
  requireThat(body, "Body required");
  const reader = body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      size += value.byteLength;
      requireThat(size <= limit, "Body too large", 413);
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  } finally {
    reader.releaseLock();
  }
  const result = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}
export async function jsonBody(request: Request): Promise<unknown> {
  try {
    return JSON.parse(
      new TextDecoder().decode(await readBounded(request.body, MAX_JSON_BYTES)),
    );
  } catch (error) {
    if (error instanceof ProtocolError) throw error;
    throw new ProtocolError(400, "Invalid JSON");
  }
}

export * from "./tasks.ts";
