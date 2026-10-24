import {
  digest,
  requireThat,
  type Assignment,
  type BuildRecord,
  type CandidateSpec,
  type EngineBuildJob,
  type Generation,
  type JobSpec,
} from "@enginebox/fleet-protocol";
type Store = { get<T>(key: string): T | undefined };
type IntegrationRecord = {
  spec: { id: string; kind: string; project: string };
  status: string;
  assignment?: { node: string };
  result?: string;
};
/** All methods are synchronous so candidate/lease checks and writes stay atomic in the DO turn. */
export class Builds {
  constructor(private store: Store) {}
  source(job: EngineBuildJob) {
    const integration = this.store.get<IntegrationRecord>(
      `job:${job.build.integration}`,
    );
    const candidate = this.store.get<{ spec: CandidateSpec }>(
      `candidate:${job.build.candidate}`,
    );
    const generation = this.store.get<{ manifest: Generation }>(
      `generation:${job.generation}`,
    )?.manifest;
    requireThat(
      integration?.spec.kind === "integration" &&
        integration.spec.project === job.project &&
        integration.status === "completed" &&
        integration.result &&
        integration.assignment,
      "Build requires a completed native integration",
      409,
    );
    const report = JSON.parse(integration.result);
    requireThat(
      candidate?.spec.id === job.build.integration &&
        candidate.spec.generation === job.generation &&
        generation?.project === job.project &&
        generation.metadata.integration === job.build.integration &&
        generation.metadata.treeHash === job.build.sourceTree &&
        report.generation === job.generation &&
        report.treeHash === job.build.sourceTree,
      "Build source differs from exact frozen integration",
      409,
    );
    return integration.assignment.node;
  }
  eligible(job: JobSpec, node: string | undefined) {
    if (job.kind !== "engine-build") return true;
    return !!node && this.source(job) === node;
  }
  assigned(job: EngineBuildJob, assignment: Assignment) {
    requireThat(
      this.source(job) === assignment.node,
      "Build must run on the integration executor until remote source import is qualified",
      409,
    );
  }
  complete(job: EngineBuildJob, result: string, node: string): BuildRecord {
    requireThat(this.source(job) === node, "Build executor changed", 409);
    const report = JSON.parse(result);
    digest(report.artifact);
    const build = this.store.get<BuildRecord>(`artifact:${report.artifact}`);
    requireThat(
      report.generation === job.generation &&
        report.sourceTree === job.build.sourceTree &&
        report.recipe === job.build.recipe &&
        build?.project === job.project &&
        build.generation === job.generation &&
        build.sourceTree === job.build.sourceTree &&
        build.recipe === job.build.recipe &&
        build.candidate === job.build.candidate &&
        build.createdBy === `node:${node}`,
      "Build result does not reference its exact published artifact",
      409,
    );
    return build;
  }
}
