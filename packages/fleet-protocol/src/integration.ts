import { digest, exactKeys, identifier, record, requireThat } from "./index.ts";
export type ChangeSubmission = {
  id: string;
  task?: string;
  project: string;
  base: string;
  generation: string;
  layerfsProject: string;
  baseRef: string;
  baseRoot: string;
  ref: string;
  root: string;
};
export type IntegrationPlan = {
  id: string;
  layerfsProject: string;
  baseRef: string;
  baseRoot: string;
  changes: {
    id: string;
    submission: string;
    generation: string;
    ref: string;
    root: string;
  }[];
  taskRequirements: Record<string, string>;
};
export type IntegrationJob = {
  id: string;
  project: string;
  generation: string;
  kind: "integration";
  integration: IntegrationPlan;
  argv?: never;
  check?: never;
  timeoutSeconds?: never;
};
export function layerfsRef(value: unknown): asserts value is string {
  requireThat(
    typeof value === "string" && /^(?:12|32)[a-f0-9]{64}$/.test(value),
    "Immutable LayerFS Commit or Layer reference required",
  );
}
export function validateSubmission(value: unknown): ChangeSubmission {
  record(value);
  exactKeys(value, [
    "id",
    "task",
    "project",
    "base",
    "generation",
    "layerfsProject",
    "baseRef",
    "baseRoot",
    "ref",
    "root",
  ]);
  identifier(value.id);
  if (value.task !== undefined) identifier(value.task);
  identifier(value.project);
  identifier(value.layerfsProject);
  digest(value.base);
  digest(value.generation);
  digest(value.baseRoot);
  digest(value.root);
  layerfsRef(value.baseRef);
  layerfsRef(value.ref);
  return value as ChangeSubmission;
}
export function validateIntegration(value: unknown): IntegrationPlan {
  record(value);
  exactKeys(value, [
    "id",
    "layerfsProject",
    "baseRef",
    "baseRoot",
    "changes",
    "taskRequirements",
  ]);
  identifier(value.id);
  identifier(value.layerfsProject);
  layerfsRef(value.baseRef);
  digest(value.baseRoot);
  requireThat(
    Array.isArray(value.changes) &&
      value.changes.length > 0 &&
      value.changes.length <= 128,
    "Integration requires 1–128 changes",
  );
  const ids = new Set<string>();
  for (const change of value.changes) {
    record(change);
    exactKeys(change, ["id", "submission", "generation", "ref", "root"]);
    identifier(change.id);
    identifier(change.submission);
    digest(change.generation);
    layerfsRef(change.ref);
    digest(change.root);
    requireThat(!ids.has(change.id), "Duplicate change");
    ids.add(change.id);
  }
  record(value.taskRequirements);
  for (const [id, hash] of Object.entries(value.taskRequirements)) {
    requireThat(ids.has(id), "Unrelated task requirements");
    digest(hash);
  }
  return value as IntegrationPlan;
}
