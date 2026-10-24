import {
  digest,
  exactKeys,
  identifier,
  integer,
  record,
  requireThat,
} from "./index.ts";

export const MACHINE_INVITATION_MS = 10 * 60_000;
export const MACHINE_LEASE_MS = 60_000;
export const MACHINE_MAX_LIFETIME_MS = 60 * 60_000;
export const MACHINE_MAX_SIGNALS = 258;
export type NativeSettings = {
  label: string;
  ownerAccountId: string;
  allowControl: boolean;
};
export type MachineMode = "watch" | "control";
export type MachineGrantRequest = {
  id: string;
  sessionId: string;
  nodeId: string;
  actorId: string;
  mode: MachineMode;
};
export type MachineSession = {
  id: string;
  project: string;
  nodeId: string;
  nodeEpoch: number;
  actorId: string;
  mode: MachineMode;
  state: "open" | "ended";
  expires: number;
  createdAt: number;
  endedAt?: number;
  reason?: "client" | "host" | "owner" | "expired" | "fenced" | "revoked";
};
export type MachineSignal =
  | { type: "offer" | "answer"; sdp: string }
  | {
      type: "ice";
      candidate: {
        candidate: string;
        sdpMid: string | null;
        sdpMLineIndex: number | null;
      } | null;
    };
export type MachineSignalRequest = { id: string; signal: MachineSignal };
export type MachineSignalEntry = MachineSignalRequest & {
  sequence: number;
  side: "client" | "host";
};

function text(
  value: unknown,
  max: number,
  name: string,
): asserts value is string {
  requireThat(
    typeof value === "string" &&
      value.length > 0 &&
      new TextEncoder().encode(value).byteLength <= max &&
      !/[\x00-\x1f\x7f]/.test(value),
    `Invalid ${name}`,
  );
}
export function validateNativeSettings(value: unknown): NativeSettings {
  record(value);
  exactKeys(value, ["label", "ownerAccountId", "allowControl"]);
  text(value.label, 120, "machine label");
  text(value.ownerAccountId, 128, "owner account");
  requireThat(
    typeof value.allowControl === "boolean",
    "Machine control policy required",
  );
  return {
    label: value.label,
    ownerAccountId: value.ownerAccountId,
    allowControl: value.allowControl,
  };
}
export function validateMachineInvitation(value: unknown) {
  record(value);
  exactKeys(value, ["id", "label", "ownerAccountId", "allowControl"]);
  identifier(value.id);
  return {
    id: value.id,
    native: validateNativeSettings({
      label: value.label,
      ownerAccountId: value.ownerAccountId,
      allowControl: value.allowControl,
    }),
  };
}
export function validateMachineGrant(value: unknown): MachineGrantRequest {
  record(value);
  exactKeys(value, ["id", "sessionId", "nodeId", "actorId", "mode"]);
  identifier(value.id);
  identifier(value.sessionId);
  identifier(value.nodeId);
  text(value.actorId, 128, "stream actor");
  requireThat(
    value.mode === "watch" || value.mode === "control",
    "Invalid stream mode",
  );
  return {
    id: value.id,
    sessionId: value.sessionId,
    nodeId: value.nodeId,
    actorId: value.actorId,
    mode: value.mode,
  };
}
export function machineGrantToken(value: unknown): asserts value is string {
  requireThat(
    typeof value === "string" &&
      /^[a-z0-9][a-z0-9-]{0,47}\.stream\.[a-f0-9]{64}$/.test(value),
    "Stream grant required",
    403,
  );
}
export function validateMachineOpen(value: unknown) {
  record(value);
  exactKeys(value, ["id", "grant", "clientSecretHash"]);
  identifier(value.id);
  machineGrantToken(value.grant);
  digest(value.clientSecretHash);
  return {
    id: value.id,
    grant: value.grant,
    clientSecretHash: value.clientSecretHash,
  };
}
export function validateMachineSignal(value: unknown): MachineSignalRequest {
  record(value);
  exactKeys(value, ["id", "signal"]);
  identifier(value.id);
  record(value.signal);
  const signal = value.signal;
  if (signal.type === "offer" || signal.type === "answer") {
    exactKeys(signal, ["type", "sdp"]);
    requireThat(
      typeof signal.sdp === "string" &&
        /^v=0\r?\n/.test(signal.sdp) &&
        !signal.sdp.includes("\0") &&
        new TextEncoder().encode(signal.sdp).byteLength <= 64 * 1024,
      "Invalid or oversized SDP",
    );
    return { id: value.id, signal: { type: signal.type, sdp: signal.sdp } };
  }
  requireThat(signal.type === "ice", "Only SDP and ICE signaling is accepted");
  exactKeys(signal, ["type", "candidate"]);
  if (signal.candidate === null)
    return { id: value.id, signal: { type: "ice", candidate: null } };
  record(signal.candidate);
  const candidate = signal.candidate;
  exactKeys(candidate, ["candidate", "sdpMid", "sdpMLineIndex"]);
  text(candidate.candidate, 4096, "ICE candidate");
  if (candidate.sdpMid !== null) text(candidate.sdpMid, 128, "ICE media ID");
  if (candidate.sdpMLineIndex !== null)
    integer(candidate.sdpMLineIndex, 0, 65535);
  return {
    id: value.id,
    signal: {
      type: "ice",
      candidate: {
        candidate: candidate.candidate,
        sdpMid: candidate.sdpMid,
        sdpMLineIndex: candidate.sdpMLineIndex,
      },
    },
  };
}
