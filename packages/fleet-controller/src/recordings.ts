import { createHash } from "node:crypto";
import {
  canonical,
  DEFAULT_PLAYTEST_POLICY,
  digest,
  exactKeys,
  identifier,
  integer,
  RECORDING_LIMITS,
  recordingText,
  record,
  requireThat,
  validateRecordingFinish,
  type BuildRecord,
  type CandidateSpec,
  type MediaFinalizeJob,
  type PlaytestPolicy,
  type Recording,
  type RecordingChunk,
  type RecordingEvent,
  type RecordingMedia,
  type Task,
  type TestSession,
} from "@enginebox/fleet-protocol";

type Store = {
  get<T>(key: string): T | undefined;
  put(key: string, value: unknown): void;
  rows<T>(prefix: string): T[];
};
type Candidate = {
  spec: CandidateSpec;
  taskRequirements?: Record<string, string>;
};
const day = 86_400_000;
function requirements(task: Task) {
  return createHash("sha256")
    .update(
      canonical({
        title: task.title,
        description: task.description,
        type: task.type,
      }),
    )
    .digest("hex");
}
export function recordingSummary(value: Recording) {
  const { chunks, output, ...summary } = value;
  return {
    ...summary,
    uploadedChunks: chunks.filter((chunk) => chunk.uploaded).length,
    eventCount: value.events.length,
  };
}
export class Recordings {
  constructor(
    private store: Store,
    private sql: SqlStorage,
  ) {}
  policy() {
    return (
      this.store.get<PlaytestPolicy>("playtest-policy") ??
      DEFAULT_PLAYTEST_POLICY
    );
  }
  configure(policy: PlaytestPolicy) {
    requireThat(
      !this.store.get("pendingDecision"),
      "Acceptance must settle before changing evidence policy",
      409,
    );
    this.store.put("playtest-policy", policy);
    return policy;
  }
  contexts() {
    const rows = this.sql
      .exec<{
        value: string;
      }>("SELECT value FROM records WHERE key LIKE 'task:%' AND json_extract(value,'$.status')='testing' ORDER BY key LIMIT 100")
      .toArray();
    return rows.flatMap(({ value }) => {
      const task = JSON.parse(value) as Task;
      if (!task.candidate || !task.resultGeneration) return [];
      const candidate = this.store.get<Candidate>(
        `candidate:${task.candidate}`,
      );
      if (candidate?.taskRequirements?.[task.id] !== requirements(task))
        return [];
      const builds = this.sql
        .exec<{ value: string }>(
          "SELECT value FROM records WHERE key LIKE 'artifact:%' AND json_extract(value,'$.candidate')=? ORDER BY key LIMIT 32",
          task.candidate,
        )
        .toArray()
        .map((row) => JSON.parse(row.value) as BuildRecord);
      const build = builds.find(
        (build) =>
          build.toolchain.target === "web" &&
          build.generation === candidate.spec.generation,
      );
      return build
        ? [
            {
              taskId: task.id,
              taskRevision: task.revision,
              title: task.title,
              candidate: task.candidate,
              generation: build.generation,
              artifact: build.id,
              engine: build.toolchain.engine,
            },
          ]
        : [];
    });
  }
  session(id: string) {
    return this.store.get<TestSession>(`test-session:${id}`) ?? null;
  }
  createSession(input: unknown, actor: string) {
    record(input);
    exactKeys(input, [
      "id",
      "taskId",
      "expectedRevision",
      "tester",
      "artifact",
    ]);
    identifier(input.id);
    identifier(input.taskId);
    integer(input.expectedRevision, 1, Number.MAX_SAFE_INTEGER);
    recordingText(input.tester, 128);
    requireThat(input.tester.length > 0, "Tester required");
    if (input.artifact !== undefined) digest(input.artifact);
    const prior = this.session(input.id);
    if (prior) {
      requireThat(
        prior.actor === actor &&
          prior.taskId === input.taskId &&
          prior.taskRevision === input.expectedRevision &&
          prior.tester === input.tester &&
          (!input.artifact || prior.artifact === input.artifact),
        "Test session identity differs",
        409,
      );
      return prior;
    }
    const task = this.store.get<Task>(`task:${input.taskId}`);
    requireThat(
      task?.status === "testing" &&
        task.revision === input.expectedRevision &&
        task.candidate &&
        task.resultGeneration,
      "Open the current tested task revision",
      409,
    );
    const candidate = this.store.get<Candidate>(`candidate:${task.candidate}`),
      expected = requirements(task);
    requireThat(
      candidate?.taskRequirements?.[task.id] === expected &&
        candidate.spec.changes.some(
          (change) =>
            change.id === task.id &&
            change.generation === task.resultGeneration,
        ),
      "Candidate no longer matches task requirements",
      409,
    );
    const choice =
      input.artifact ??
      this.contexts().find((context) => context.taskId === task.id)?.artifact;
    const build =
      typeof choice === "string"
        ? this.store.get<BuildRecord>(`artifact:${choice}`)
        : undefined;
    requireThat(
      build?.candidate === task.candidate &&
        build.generation === candidate.spec.generation &&
        build.toolchain.target === "web",
      "This task needs a published Web build",
      409,
    );
    const session: TestSession = {
      id: input.id,
      project: task.project,
      taskId: task.id,
      taskRevision: task.revision,
      taskGeneration: task.resultGeneration,
      requirements: expected,
      candidate: task.candidate,
      generation: build.generation,
      artifact: build.id,
      engine: build.toolchain.engine,
      tester: input.tester,
      actor,
      createdAt: Date.now(),
      expiresAt: Date.now() + 60 * 60_000,
    };
    this.store.put(`test-session:${session.id}`, session);
    return session;
  }
  get(id: string) {
    return this.store.get<Recording>(`recording:${id}`) ?? null;
  }
  list(taskId?: string) {
    const query = taskId
      ? "SELECT value FROM records WHERE key LIKE 'recording:%' AND json_extract(value,'$.taskId')=? ORDER BY rowid DESC LIMIT 30"
      : "SELECT value FROM records WHERE key LIKE 'recording:%' ORDER BY rowid DESC LIMIT 30";
    return this.sql
      .exec<{ value: string }>(query, ...(taskId ? [taskId] : []))
      .toArray()
      .map((row) => recordingSummary(JSON.parse(row.value) as Recording));
  }
  create(input: unknown, actor: string) {
    record(input);
    exactKeys(input, [
      "id",
      "testSessionId",
      "mimeType",
      "captureOrigin",
      "gameAudio",
      "microphone",
    ]);
    identifier(input.id);
    identifier(input.testSessionId);
    requireThat(
      typeof input.mimeType === "string" &&
        input.mimeType.length <= 128 &&
        /^video\/(webm|mp4)(?:;[a-zA-Z0-9=., _"-]+)?$/.test(input.mimeType),
      "Unsupported recording format",
    );
    requireThat(
      input.captureOrigin === "browser-webrtc" ||
        input.captureOrigin === "browser-canvas",
      "Invalid capture origin",
    );
    requireThat(
      typeof input.gameAudio === "boolean" &&
        typeof input.microphone === "boolean",
      "Capture audio choices required",
    );
    const prior = this.get(input.id);
    if (prior) {
      requireThat(
        prior.actor === actor &&
          prior.testSessionId === input.testSessionId &&
          prior.mimeType === input.mimeType &&
          prior.captureOrigin === input.captureOrigin &&
          prior.gameAudio === input.gameAudio &&
          prior.microphone === input.microphone,
        "Recording identity differs",
        409,
      );
      return prior;
    }
    const session = this.session(input.testSessionId);
    requireThat(
      session && session.actor === actor && session.expiresAt > Date.now(),
      "Test session is unavailable",
      409,
    );
    const active = this.sql
      .exec<{
        n: number;
      }>("SELECT COUNT(*) AS n FROM records WHERE key LIKE 'recording:%' AND json_extract(value,'$.actor')=? AND json_extract(value,'$.state') IN ('recording','uploading','finalizing') AND json_extract(value,'$.uploadExpiresAt')>?", actor, Date.now())
      .one().n;
    requireThat(active < 8, "Too many active recordings for this service", 429);
    const value: Recording = {
      ...session,
      id: input.id,
      testSessionId: session.id,
      state: "recording",
      mimeType: input.mimeType,
      captureOrigin: input.captureOrigin,
      gameAudio: input.gameAudio,
      microphone: input.microphone,
      chunks: [],
      output: [],
      events: [],
      bytes: 0,
      finish: null,
      media: null,
      error: null,
      createdAt: Date.now(),
      expiresAt: Date.now() + 7 * day,
      uploadExpiresAt: Math.min(session.expiresAt, Date.now() + 40 * 60_000),
      finishedAt: null,
    };
    this.store.put(`recording:${value.id}`, value);
    return value;
  }
  private owned(id: string, actor: string) {
    const value = this.get(id);
    requireThat(value, "Unknown recording", 404);
    requireThat(
      value.actor === actor,
      "Recording belongs to another service",
      403,
    );
    return value;
  }
  private open(value: Recording) {
    requireThat(
      ["recording", "uploading"].includes(value.state) &&
        value.uploadExpiresAt > Date.now(),
      "Recording upload is closed",
      409,
    );
  }
  reserve(
    id: string,
    chunk: Omit<RecordingChunk, "uploaded">,
    actor: string,
    output = false,
  ) {
    const value = output ? this.get(id) : this.owned(id, actor);
    requireThat(value, "Unknown recording", 404);
    if (output)
      requireThat(
        value.state === "finalizing",
        "Recording is not awaiting finalization",
        409,
      );
    else this.open(value);
    integer(chunk.sequence, 0, output ? 127 : RECORDING_LIMITS.maxChunks - 1);
    integer(chunk.segment, 0, RECORDING_LIMITS.maxSegments - 1);
    digest(chunk.sha256);
    integer(chunk.bytes, 1, RECORDING_LIMITS.maxChunkBytes);
    const list = output ? value.output : value.chunks,
      prior = list.find((item) => item.sequence === chunk.sequence);
    if (prior) {
      requireThat(
        prior.sha256 === chunk.sha256 &&
          prior.bytes === chunk.bytes &&
          prior.segment === chunk.segment,
        "Chunk sequence already has different data",
        409,
      );
      return prior;
    }
    requireThat(
      list.reduce((sum, item) => sum + item.bytes, 0) + chunk.bytes <=
        RECORDING_LIMITS.maxBytes + (output ? 16 * 1024 * 1024 : 0),
      "Recording byte limit reached",
      413,
    );
    const result = { ...chunk, uploaded: false };
    list.push(result);
    list.sort((a, b) => a.sequence - b.sequence);
    if (!output) {
      value.bytes += chunk.bytes;
      value.state = "uploading";
    }
    this.store.put(`recording:${id}`, value);
    return result;
  }
  acknowledge(id: string, sequence: number, hash: string, output = false) {
    const value = this.get(id);
    requireThat(value, "Unknown recording", 404);
    requireThat(value.state !== "expired", "Recording expired", 410);
    if (!output)
      requireThat(
        ["recording", "uploading", "finalizing"].includes(value.state),
        "Input upload is closed",
        409,
      );
    const chunk = (output ? value.output : value.chunks).find(
      (chunk) => chunk.sequence === sequence,
    );
    requireThat(chunk?.sha256 === hash, "Chunk reservation changed", 409);
    chunk.uploaded = true;
    this.store.put(`recording:${id}`, value);
    return value;
  }
  events(id: string, input: unknown, actor: string) {
    const value = this.owned(id, actor);
    record(input);
    exactKeys(input, ["operationId", "events"]);
    identifier(input.operationId);
    const key = `recording-op:${id}:${input.operationId}`,
      fingerprint = canonical(input),
      prior = this.store.get<string>(key);
    if (prior) {
      requireThat(
        prior === fingerprint,
        "Recording event request changed",
        409,
      );
      return value;
    }
    this.open(value);
    requireThat(
      Array.isArray(input.events) &&
        input.events.length > 0 &&
        input.events.length <= 128 &&
        value.events.length + input.events.length <= 512,
      "Too many recording events",
    );
    const events = input.events.map((event) => {
      record(event);
      exactKeys(event, ["atMs", "type", "text"]);
      integer(event.atMs, 0, RECORDING_LIMITS.maxDurationMs + 10_000);
      requireThat(
        event.type === "marker" || event.type === "gap",
        "Invalid recording event",
      );
      recordingText(event.text, 500);
      return event as RecordingEvent;
    });
    value.events.push(...events);
    this.store.put(`recording:${id}`, value);
    this.store.put(key, fingerprint);
    return value;
  }
  finish(
    id: string,
    input: unknown,
    actor: string,
  ): { recording: Recording; job: MediaFinalizeJob | null } {
    const value = this.owned(id, actor),
      finish = validateRecordingFinish(input);
    const job: MediaFinalizeJob = {
      id: `media-${createHash("sha256").update(id).digest("hex").slice(0, 40)}`,
      project: value.project,
      generation: value.generation,
      kind: "media-finalize",
      recording: id,
      timeoutSeconds: 900,
    };
    if (value.finish) {
      requireThat(
        canonical(value.finish) === canonical(finish),
        "Recording finish identity differs",
        409,
      );
      return { recording: value, job: value.bytes === 0 ? null : job };
    }
    this.open(value);
    if (finish.bytes === 0 && value.chunks.length === 0) {
      value.finish = finish;
      value.state = "partial";
      value.finishedAt = Date.now();
      this.store.put(`recording:${id}`, value);
      return { recording: value, job: null };
    }
    requireThat(
      value.bytes === finish.bytes &&
        value.chunks.length > 0 &&
        value.chunks.every((chunk) => chunk.uploaded),
      "Recording still has unacknowledged chunks",
      409,
    );
    const sequences = finish.segments.flatMap((segment) => segment.chunks);
    requireThat(
      sequences.length === value.chunks.length &&
        sequences.every((sequence, index) => sequence === index),
      "Recording chunks must be complete and ordered",
      409,
    );
    for (const segment of finish.segments)
      for (const sequence of segment.chunks)
        requireThat(
          value.chunks[sequence]?.segment === segment.index,
          "Recording segment identity differs",
          409,
        );
    value.finish = finish;
    value.state = "finalizing";
    value.finishedAt = Date.now();
    this.store.put(`recording:${id}`, value);
    return { recording: value, job };
  }
  complete(id: string, result: unknown, ok: boolean) {
    const value = this.get(id);
    requireThat(
      value?.state === "finalizing",
      "Recording is not finalizing",
      409,
    );
    record(result);
    if (!ok) {
      value.state = "failed";
      value.error =
        typeof result.error === "string"
          ? result.error.slice(0, 500)
          : "Recording validation failed";
      this.store.put(`recording:${id}`, value);
      return value;
    }
    requireThat(
      result.recording === id,
      "Decoded result names another recording",
      409,
    );
    const media = result.media;
    record(media);
    exactKeys(media, [
      "sha256",
      "bytes",
      "chunks",
      "durationMs",
      "width",
      "height",
      "hasAudio",
      "mimeType",
    ]);
    digest(media.sha256);
    integer(media.bytes, 1, RECORDING_LIMITS.maxBytes + 16 * 1024 * 1024);
    integer(media.durationMs, 1, RECORDING_LIMITS.maxDurationMs + 10_000);
    integer(media.width, 1, 3840);
    integer(media.height, 1, 2160);
    requireThat(
      typeof media.hasAudio === "boolean" &&
        (media.mimeType === "video/webm" || media.mimeType === "video/mp4"),
      "Invalid decoded media",
    );
    requireThat(
      Array.isArray(media.chunks) &&
        media.chunks.length > 0 &&
        media.chunks.length === value.output.length,
      "Output chunk count differs",
      409,
    );
    let bytes = 0;
    for (let sequence = 0; sequence < media.chunks.length; sequence++) {
      const chunk = media.chunks[sequence];
      record(chunk);
      exactKeys(chunk, ["sequence", "sha256", "bytes"]);
      const stored = value.output[sequence];
      requireThat(
        chunk.sequence === sequence &&
          stored?.uploaded &&
          stored.sequence === sequence &&
          stored.sha256 === chunk.sha256 &&
          stored.bytes === chunk.bytes,
        "Decoded output has not been durably uploaded",
        409,
      );
      bytes += stored.bytes;
    }
    requireThat(bytes === media.bytes, "Decoded media size differs", 409);
    value.media = media as RecordingMedia;
    value.state =
      value.finish?.outcome === "partial" ||
      value.events.some((event) => event.type === "gap")
        ? "partial"
        : "ready";
    value.expiresAt = Date.now() + this.policy().retentionDays * day;
    value.error = null;
    this.store.put(`recording:${id}`, value);
    return value;
  }
  private evidence(task: Task) {
    const policy = this.policy();
    const rows = this.sql
      .exec<{
        value: string;
      }>("SELECT value FROM records WHERE key LIKE 'recording:%' AND json_extract(value,'$.taskId')=? AND json_extract(value,'$.state')='ready' ORDER BY rowid DESC LIMIT 100", task.id)
      .toArray();
    return rows
      .map((row) => JSON.parse(row.value) as Recording)
      .find(
        (value) =>
          value.expiresAt > Date.now() &&
          value.candidate === task.candidate &&
          value.taskGeneration === task.resultGeneration &&
          value.requirements === requirements(task) &&
          value.finish?.outcome === "pass" &&
          value.media &&
          value.media.durationMs >= policy.minDurationMs &&
          (!policy.requireAudio || (value.media.hasAudio && value.gameAudio)),
      );
  }
  ready(task: Task) {
    return !this.policy().required || Boolean(this.evidence(task));
  }
  requireCandidate(candidate: Candidate) {
    const policy = this.policy(),
      recordings: {
        task: string;
        recording: string;
        artifact: string;
        mediaSha256: string;
        tester: string;
        durationMs: number;
      }[] = [];
    if (policy.required)
      for (const change of candidate.spec.changes) {
        const task = this.store.get<Task>(`task:${change.id}`);
        if (!task) continue;
        const evidence = this.evidence(task);
        requireThat(
          task.candidate === candidate.spec.id &&
            task.resultGeneration === change.generation &&
            evidence?.media,
          "A task needs current playable passing evidence",
          409,
        );
        recordings.push({
          task: task.id,
          recording: evidence.id,
          artifact: evidence.artifact,
          mediaSha256: evidence.media.sha256,
          tester: evidence.tester,
          durationMs: evidence.media.durationMs,
        });
      }
    return { policy, recordings };
  }
  expiredBatch(cursor: string) {
    return this.sql
      .exec<{
        key: string;
        value: string;
      }>("SELECT key,value FROM records WHERE key LIKE 'recording:%' AND key>? AND NOT EXISTS (SELECT 1 FROM records cleaned WHERE cleaned.key='recording-cleaned:' || substr(records.key,11)) AND (json_extract(value,'$.expiresAt')<=? OR (json_extract(value,'$.uploadExpiresAt')<=? AND json_extract(value,'$.state') IN ('recording','uploading'))) ORDER BY key LIMIT 20", cursor, Date.now(), Date.now())
      .toArray();
  }
  expire(id: string) {
    const value = this.get(id);
    if (!value) return null;
    value.state = "expired";
    value.expiresAt = Date.now();
    value.error = "Recording retention expired";
    this.store.put(`recording:${id}`, value);
    return value;
  }
}
