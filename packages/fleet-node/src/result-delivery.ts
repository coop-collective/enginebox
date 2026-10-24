import { DatabaseSync } from "node:sqlite";
import {
  canonical,
  ProtocolError,
  requireThat,
  type Assignment,
} from "@enginebox/fleet-protocol";
import { Client } from "./client.ts";
import { UncertainNativeRun } from "./native-assistant.ts";

export type FinishedOutcome = { ok: boolean; result: Record<string, unknown> };
type JobView = {
  spec: { id: string; project: string };
  status: string;
  assignment?: Pick<Assignment, "attempt" | "node" | "epoch" | "session">;
  result?: string;
};
function outcome(value: unknown): FinishedOutcome {
  requireThat(
    value !== null && typeof value === "object",
    "Invalid finished outcome",
  );
  const saved = value as FinishedOutcome;
  requireThat(
    typeof saved.ok === "boolean" &&
      saved.result !== null &&
      typeof saved.result === "object" &&
      !Array.isArray(saved.result),
    "Invalid finished outcome",
  );
  return saved;
}
function save(
  db: DatabaseSync,
  assignment: Assignment,
  value: FinishedOutcome,
) {
  const changed = db
    .prepare(
      "UPDATE operations SET result=? WHERE attempt=? AND state='finished'",
    )
    .run(canonical(value), assignment.attempt);
  requireThat(
    changed.changes === 1,
    "Finished journal receipt disappeared",
    409,
  );
}
/** Only delivers an already-settled executor receipt. It never starts native work.
 * A controller validation conflict is not evidence of an uncertain executor.
 * Conversely, transport/auth failures are not evidence that validation failed.
 */
export async function deliverFinishedResult(
  client: Client,
  assignment: Assignment,
  db: DatabaseSync,
): Promise<FinishedOutcome> {
  const row = db
    .prepare("SELECT state,assignment,result FROM operations WHERE attempt=?")
    .get(assignment.attempt) as
    | { state: string; assignment: string; result: string | null }
    | undefined;
  if (!row || row.state !== "finished" || !row.result)
    throw new UncertainNativeRun(
      "Only a definitively finished executor may deliver a terminal result",
    );
  const prior = JSON.parse(row.assignment) as Assignment;
  requireThat(
    prior.id === assignment.id &&
      prior.project === assignment.project &&
      prior.node === assignment.node &&
      prior.epoch === assignment.epoch &&
      prior.session === assignment.session &&
      prior.attempt === assignment.attempt,
    "Finished receipt belongs to another assignment",
    409,
  );
  const saved = outcome(JSON.parse(row.result));
  const send = (value: FinishedOutcome) =>
    client.json("/v1/node/finish", { assignment, ...value });
  try {
    await send(saved);
    return saved;
  } catch (error) {
    if (!(error instanceof ProtocolError) || ![400, 409].includes(error.status))
      throw error;
    const job = await client.json<JobView>(
      `/v1/projects/${assignment.project}/jobs/${assignment.id}`,
    );
    requireThat(
      job?.spec?.id === assignment.id &&
        job.spec.project === assignment.project &&
        job.assignment?.attempt === assignment.attempt &&
        job.assignment.node === assignment.node &&
        job.assignment.epoch === assignment.epoch &&
        job.assignment.session === assignment.session,
      "Controller result belongs to a different attempt; retain the reservation",
      409,
    );
    if (job.status === "completed" || job.status === "failed") {
      // Promotion/queue callbacks can fail after the job outcome committed.
      // Adopt that immutable receipt, never downgrade a committed success.
      requireThat(
        typeof job.result === "string",
        "Controller terminal receipt is missing",
        409,
      );
      const authoritative = outcome({
        ok: job.status === "completed",
        result: JSON.parse(job.result),
      });
      save(db, assignment, authoritative);
      await send(authoritative);
      return authoritative;
    }
    requireThat(
      job.status === "assigned",
      "Controller job is not assigned to this executor; retain the reservation",
      409,
    );
    // A persisted failure is already the exact retry receipt. If even that is
    // rejected, keep it intact for owner reconciliation rather than rewriting
    // its diagnostics on every poll.
    if (!saved.ok) throw error;
    const failed: FinishedOutcome = {
      ok: false,
      result: {
        error: `Controller rejected the completed executor result (${error.status}); a new job must validate the current candidate. ${error.message.slice(0, 1600)}`,
      },
    };
    // Persist before sending. A lost response or node restart retries this exact
    // failure receipt instead of resubmitting the rejected success/native work.
    save(db, assignment, failed);
    await send(failed);
    return failed;
  }
}
