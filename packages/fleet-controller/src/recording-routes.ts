import {
  digest,
  identifier,
  integer,
  jsonBody,
  readBounded,
  RECORDING_LIMITS,
  requireThat,
  sha256,
  validatePlaytestPolicy,
  type Recording,
} from "@enginebox/fleet-protocol";
import { immutableObject } from "./artifacts.ts";
import { recordingSummary } from "./recordings.ts";

export function recordingKey(
  project: string,
  id: string,
  output: boolean,
  sequence: number,
) {
  return `projects/${project}/recordings/${id}/${output ? "output" : "input"}/${sequence}`;
}
export async function recordingRoute(
  request: Request,
  env: Env,
  project: string,
  parts: string[],
  actor: string,
  owner: boolean,
  worker: string | null,
) {
  const authority = env.PROJECTS.getByName(project),
    method = request.method;
  const json = (value: unknown, status = 200) =>
    Response.json(value, {
      status,
      headers: {
        "cache-control": "no-store",
        "x-content-type-options": "nosniff",
      },
    });
  if (parts[0] === "test-contexts" && parts.length === 1 && method === "GET")
    return json({
      contexts: await authority.testContexts(),
      limits: RECORDING_LIMITS,
      policy: await authority.playtestPolicy(),
    });
  if (parts[0] === "playtest-policy" && parts.length === 1) {
    requireThat(owner, "Owner authorization required", 403);
    if (method === "PUT")
      return json(
        await authority.configurePlaytests(
          validatePlaytestPolicy(await jsonBody(request)),
        ),
      );
    if (method === "GET") return json(await authority.playtestPolicy());
  }
  if (parts[0] === "test-sessions") {
    if (parts.length === 1 && method === "POST")
      return json(
        await authority.createTestSession(await jsonBody(request), actor),
        201,
      );
    if (parts.length === 2 && method === "GET") {
      identifier(parts[1]);
      const session = await authority.testSession(parts[1]);
      requireThat(session, "Unknown test session", 404);
      requireThat(
        owner || session.actor === actor,
        "Test session belongs to another service",
        403,
      );
      return json(session);
    }
  }
  if (parts[0] === "recordings") {
    if (parts.length === 1) {
      if (method === "POST")
        return json(
          await authority.createRecording(await jsonBody(request), actor),
          201,
        );
      if (method === "GET") {
        const task =
          new URL(request.url).searchParams.get("taskId") ?? undefined;
        if (task) identifier(task);
        return json({ recordings: await authority.recordings(task) });
      }
    }
    identifier(parts[1]);
    const id = parts[1];
    if (parts.length === 2 && method === "GET") {
      const value = await authority.recording(id);
      requireThat(value, "Unknown recording", 404);
      return json(value);
    }
    if (parts.length === 3 && parts[2] === "events" && method === "POST")
      return json(
        await authority.recordingEvents(id, await jsonBody(request), actor),
      );
    if (parts.length === 3 && parts[2] === "finish" && method === "POST")
      return json(
        await authority.finishRecording(id, await jsonBody(request), actor),
      );
    if (
      parts.length === 3 &&
      parts[2] === "media" &&
      ["GET", "HEAD"].includes(method)
    ) {
      const value = await authority.recording(id);
      requireThat(
        value &&
          value.media &&
          ["ready", "partial"].includes(value.state) &&
          value.expiresAt > Date.now(),
        "Recording media is unavailable",
        404,
      );
      return recordingMedia(env.STORE, value, request);
    }
    if (parts.length === 4 && ["chunks", "output"].includes(parts[2])) {
      requireThat(/^(0|[1-9][0-9]*)$/.test(parts[3]), "Invalid chunk sequence");
      const sequence = Number(parts[3]),
        output = parts[2] === "output";
      integer(sequence, 0, output ? 127 : 2047);
      const key = recordingKey(project, id, output, sequence);
      if (method === "PUT") {
        if (output) {
          requireThat(
            worker,
            "Only the assigned media worker can publish decoded output",
            403,
          );
          await authority.mediaWorker(
            id,
            worker,
            request.headers.get("x-enginebox-attempt") ?? "",
          );
        }
        const expected = request.headers.get("x-chunk-sha256");
        digest(expected);
        const segment = output
          ? 0
          : Number(request.headers.get("x-recording-segment"));
        if (!output)
          requireThat(
            request.headers.has("x-recording-segment"),
            "Segment header required",
          );
        integer(segment, 0, 63);
        const bytes = await readBounded(
          request.body,
          RECORDING_LIMITS.maxChunkBytes,
        );
        requireThat(
          bytes.length > 0 && (await sha256(bytes)) === expected,
          "Recording chunk checksum differs",
          409,
        );
        await authority.reserveRecordingChunk(
          id,
          { sequence, segment, sha256: expected, bytes: bytes.length },
          actor,
          output,
        );
        await immutableObject(env.STORE, key, bytes, {
          sha256: expected,
          bytes: String(bytes.length),
        });
        try {
          if (output)
            await authority.mediaWorker(
              id,
              worker!,
              request.headers.get("x-enginebox-attempt") ?? "",
            );
          const recording = await authority.acknowledgeRecordingChunk(
            id,
            sequence,
            expected,
            output,
          );
          return json({
            recording: recordingSummary(recording),
            acknowledged: sequence,
          });
        } catch (error) {
          // A late upload must not resurrect bytes after retention cleanup.
          const current = await authority.recording(id);
          if (
            current?.state === "expired" ||
            (!output && current && ["ready", "partial"].includes(current.state))
          )
            await env.STORE.delete(key);
          throw error;
        }
      }
      if (method === "GET") {
        requireThat(
          worker || owner,
          "Only a trusted worker or owner can read raw chunks",
          403,
        );
        const value = await authority.recording(id);
        requireThat(
          value && value.expiresAt > Date.now() && value.state !== "expired",
          "Recording expired",
          410,
        );
        const chunk = (output ? value.output : value.chunks).find(
          (chunk) => chunk.sequence === sequence,
        );
        requireThat(chunk?.uploaded, "Unknown recording chunk", 404);
        const object = await env.STORE.get(key);
        requireThat(object, "Recording chunk missing", 404);
        return new Response(object.body, {
          headers: {
            "content-type": "application/octet-stream",
            "content-length": String(chunk.bytes),
            "cache-control": "no-store",
          },
        });
      }
    }
  }
  return json({ error: "Unknown recording route" }, 404);
}

async function recordingMedia(
  bucket: R2Bucket,
  value: Recording,
  request: Request,
) {
  const media = value.media!;
  let start = 0,
    end = media.bytes - 1,
    partial = false;
  const range = request.headers.get("range");
  if (range) {
    const match = /^bytes=(\d*)-(\d*)$/.exec(range);
    if (!match || (!match[1] && !match[2]))
      return new Response(null, {
        status: 416,
        headers: { "content-range": `bytes */${media.bytes}` },
      });
    start = match[1]
      ? Number(match[1])
      : Math.max(0, media.bytes - Number(match[2]));
    end = match[1] && match[2] ? Number(match[2]) : end;
    if (
      !Number.isSafeInteger(start) ||
      !Number.isSafeInteger(end) ||
      start > end ||
      start >= media.bytes
    )
      return new Response(null, {
        status: 416,
        headers: { "content-range": `bytes */${media.bytes}` },
      });
    end = Math.min(end, media.bytes - 1);
    partial = true;
  }
  const headers: Record<string, string> = {
    "content-type": media.mimeType,
    "content-length": String(end - start + 1),
    "accept-ranges": "bytes",
    etag: `"${media.sha256}"`,
    "cache-control": "private, no-store",
    "x-content-type-options": "nosniff",
  };
  if (partial)
    headers["content-range"] = `bytes ${start}-${end}/${media.bytes}`;
  async function* content() {
    let offset = 0;
    for (const chunk of media.chunks) {
      if (offset + chunk.bytes <= start) {
        offset += chunk.bytes;
        continue;
      }
      if (offset > end) break;
      const object = await bucket.get(
        recordingKey(value.project, value.id, true, chunk.sequence),
      );
      requireThat(object, "Recording output missing", 502);
      const bytes = await readBounded(
        object.body,
        RECORDING_LIMITS.maxChunkBytes,
      );
      requireThat(
        bytes.length === chunk.bytes && (await sha256(bytes)) === chunk.sha256,
        "Recording output corrupt",
        502,
      );
      yield bytes.slice(
        Math.max(0, start - offset),
        Math.min(bytes.length, end - offset + 1),
      );
      offset += chunk.bytes;
    }
  }
  const source = content();
  const body =
    request.method === "HEAD"
      ? null
      : new ReadableStream<Uint8Array>({
          async pull(controller) {
            try {
              const next = await source.next();
              if (next.done) controller.close();
              else controller.enqueue(next.value);
            } catch (error) {
              controller.error(error);
            }
          },
          async cancel() {
            await source.return();
          },
        });
  return new Response(body, { status: partial ? 206 : 200, headers });
}
