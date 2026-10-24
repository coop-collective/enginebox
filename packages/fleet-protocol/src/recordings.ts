import {
  exactKeys,
  identifier,
  integer,
  record,
  requireThat,
  type BuildRecord,
} from "./index.ts";

export const RECORDING_LIMITS = {
  maxChunkBytes: 8 * 1024 * 1024,
  maxBytes: 256 * 1024 * 1024,
  maxDurationMs: 600_000,
  maxPendingBytes: 32 * 1024 * 1024,
  maxChunks: 2048,
  maxSegments: 64,
};
export type TestSession = {
  id: string;
  project: string;
  taskId: string;
  taskRevision: number;
  taskGeneration: string;
  requirements: string;
  candidate: string;
  generation: string;
  artifact: string;
  engine: BuildRecord["toolchain"]["engine"];
  tester: string;
  actor: string;
  createdAt: number;
  expiresAt: number;
};
export type RecordingState =
  | "recording"
  | "uploading"
  | "finalizing"
  | "ready"
  | "partial"
  | "failed"
  | "expired";
export type RecordingChunk = {
  sequence: number;
  segment: number;
  sha256: string;
  bytes: number;
  uploaded: boolean;
};
export type RecordingEvent = {
  atMs: number;
  type: "marker" | "gap";
  text: string;
};
export type RecordingSegment = {
  index: number;
  chunks: number[];
  durationMs: number;
  offsetMs: number;
};
export type RecordingFinish = {
  operationId: string;
  outcome: "pass" | "fail" | "needs_work" | "partial";
  feedback: string;
  segments: RecordingSegment[];
  durationMs: number;
  bytes: number;
};
export type RecordingMedia = {
  sha256: string;
  bytes: number;
  chunks: { sequence: number; sha256: string; bytes: number }[];
  durationMs: number;
  width: number;
  height: number;
  hasAudio: boolean;
  mimeType: "video/webm" | "video/mp4";
};
export type Recording = TestSession & {
  testSessionId: string;
  state: RecordingState;
  mimeType: string;
  captureOrigin: "browser-webrtc" | "browser-canvas";
  gameAudio: boolean;
  microphone: boolean;
  chunks: RecordingChunk[];
  output: RecordingChunk[];
  events: RecordingEvent[];
  uploadExpiresAt: number;
  bytes: number;
  finish: RecordingFinish | null;
  media: RecordingMedia | null;
  error: string | null;
  finishedAt: number | null;
};
export type MediaFinalizeJob = {
  id: string;
  project: string;
  generation: string;
  kind: "media-finalize";
  recording: string;
  timeoutSeconds: number;
  argv?: never;
  check?: never;
};
export type PlaytestPolicy = {
  required: boolean;
  requireAudio: boolean;
  minDurationMs: number;
  retentionDays: number;
};
export const DEFAULT_PLAYTEST_POLICY: PlaytestPolicy = {
  required: false,
  requireAudio: false,
  minDurationMs: 1000,
  retentionDays: 30,
};
export function recordingText(
  value: unknown,
  max: number,
): asserts value is string {
  requireThat(
    typeof value === "string" &&
      value.length <= max &&
      !/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/.test(value),
    "Invalid recording text",
  );
}
export function validatePlaytestPolicy(value: unknown): PlaytestPolicy {
  record(value);
  exactKeys(value, [
    "required",
    "requireAudio",
    "minDurationMs",
    "retentionDays",
  ]);
  requireThat(
    typeof value.required === "boolean" &&
      typeof value.requireAudio === "boolean",
    "Invalid evidence policy",
  );
  integer(value.minDurationMs, 1000, RECORDING_LIMITS.maxDurationMs);
  integer(value.retentionDays, 1, 365);
  return value as PlaytestPolicy;
}
export function validateRecordingFinish(value: unknown): RecordingFinish {
  record(value);
  exactKeys(value, [
    "operationId",
    "outcome",
    "feedback",
    "segments",
    "durationMs",
    "bytes",
  ]);
  identifier(value.operationId);
  requireThat(
    ["pass", "fail", "needs_work", "partial"].includes(String(value.outcome)),
    "Choose a test outcome",
  );
  recordingText(value.feedback, 8000);
  integer(
    value.durationMs,
    value.outcome === "partial" ? 0 : 1,
    RECORDING_LIMITS.maxDurationMs + 10_000,
  );
  integer(
    value.bytes,
    value.outcome === "partial" ? 0 : 1,
    RECORDING_LIMITS.maxBytes,
  );
  requireThat(
    Array.isArray(value.segments) &&
      (value.segments.length > 0 ||
        (value.outcome === "partial" &&
          value.bytes === 0 &&
          value.durationMs === 0)) &&
      value.segments.length <= RECORDING_LIMITS.maxSegments,
    "Invalid recording segments",
  );
  const seen = new Set<number>();
  const segments = value.segments.map((segment, index) => {
    record(segment);
    exactKeys(segment, ["index", "chunks", "durationMs", "offsetMs"]);
    requireThat(segment.index === index, "Segments must be ordered");
    integer(segment.durationMs, 1, RECORDING_LIMITS.maxDurationMs + 10_000);
    integer(segment.offsetMs, 0, RECORDING_LIMITS.maxDurationMs + 10_000);
    requireThat(
      Array.isArray(segment.chunks) && segment.chunks.length > 0,
      "A segment needs uploaded chunks",
    );
    const chunks = segment.chunks.map((sequence) => {
      integer(sequence, 0, RECORDING_LIMITS.maxChunks - 1);
      requireThat(!seen.has(sequence), "Chunk used more than once");
      seen.add(sequence);
      return sequence;
    });
    return {
      index,
      chunks,
      durationMs: segment.durationMs,
      offsetMs: segment.offsetMs,
    };
  });
  return { ...value, segments } as RecordingFinish;
}
