import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, mkdir, readdir } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { executeMedia, mediaCommand } from "../src/media.ts";
import { Client } from "../src/client.ts";
import type {
  Assignment,
  Recording,
  RecordingMedia,
} from "@enginebox/fleet-protocol";

const ffmpeg =
  process.env.EBOX_TEST_FFMPEG ??
  (process.platform === "darwin"
    ? "/opt/homebrew/bin/ffmpeg"
    : "/usr/bin/ffmpeg");
const ffprobe =
  process.env.EBOX_TEST_FFPROBE ??
  (process.platform === "darwin"
    ? "/opt/homebrew/bin/ffprobe"
    : "/usr/bin/ffprobe");
const available =
  process.platform !== "win32" &&
  spawnSync(ffmpeg, ["-version"]).status === 0 &&
  spawnSync(ffprobe, ["-version"]).status === 0;
const hash = (bytes: Uint8Array) =>
  createHash("sha256").update(bytes).digest("hex");

test(
  "real FFmpeg finalizer verifies chunks, remuxes and decodes AV, and cancels without leftover files",
  { skip: !available },
  async () => {
    const root = await mkdtemp(join(tmpdir(), "enginebox-media-proof-"));
    try {
      const signal = new AbortController().signal;
      await mediaCommand(
        ffmpeg,
        [
          "-hide_banner",
          "-loglevel",
          "error",
          "-f",
          "lavfi",
          "-i",
          "testsrc2=size=320x180:rate=15",
          "-f",
          "lavfi",
          "-i",
          "sine=frequency=440:sample_rate=48000",
          "-t",
          "1.5",
          "-c:v",
          "libvpx",
          "-deadline",
          "realtime",
          "-c:a",
          "libopus",
          "source.webm",
        ],
        root,
        signal,
      );
      const bytes = await readFile(join(root, "source.webm"));
      const input = [bytes.subarray(0, 1000), bytes.subarray(1000)];
      const recording = {
        id: "record-one",
        project: "demo",
        generation: "a".repeat(64),
        state: "finalizing",
        bytes: bytes.length,
        mimeType: "video/webm",
        chunks: input.map((bytes, sequence) => ({
          sequence,
          segment: 0,
          bytes: bytes.length,
          sha256: hash(bytes),
          uploaded: true,
        })),
        finish: {
          segments: [
            { index: 0, chunks: [0, 1], durationMs: 1500, offsetMs: 0 },
          ],
        },
      } as Recording;
      const outputs: Buffer[] = [];
      class LocalController extends Client {
        override async json<T>(): Promise<T> {
          return structuredClone(recording) as T;
        }
        override async request(path: string, init: RequestInit = {}) {
          if(path===`/v1/projects/demo/recordings/${recording.id}`)return Response.json(structuredClone(recording));
          const sequence = Number(path.split("/").at(-1));
          if (init.method === "PUT") {
            const bytes = Buffer.from(init.body as Uint8Array);
            assert.equal(
              new Headers(init.headers).get("x-chunk-sha256"),
              hash(bytes),
            );
            outputs[sequence] = bytes;
            return Response.json({ acknowledged: sequence });
          }
          return new Response(input[sequence]);
        }
      }
      const assignment: Assignment = {
        id: "media-job",
        project: "demo",
        generation: recording.generation,
        kind: "media-finalize",
        recording: recording.id,
        timeoutSeconds: 60,
        node: "worker",
        epoch: 1,
        session: crypto.randomUUID(),
        expires: Date.now() + 30_000,
        attempt: crypto.randomUUID(),
      };
      const client = new LocalController("http://localhost", "no-credential"),
        state = join(root, "state"),
        config = { ffmpeg, ffprobe, projects: ["demo"] };
      const result = await executeMedia(
          config,
          assignment,
          client,
          state,
          signal,
        ),
        media = result.media as RecordingMedia;
      assert.equal(media.hasAudio, true);
      assert.equal(media.width, 320);
      assert.equal(media.height, 180);
      assert.ok(media.durationMs >= 1400 && media.durationMs < 1700);
      assert.equal(media.sha256, hash(Buffer.concat(outputs)));
      assert.equal(media.bytes, Buffer.concat(outputs).length);
      assert.deepEqual(await readdir(join(state, "media")), []);
      input[0] = Buffer.from("corrupt media bytes");
      await assert.rejects(
        executeMedia(config, assignment, client, state, signal),
        /checksum differs/,
      );
      assert.deepEqual(await readdir(join(state, "media")), []);
      await assert.rejects(
        executeMedia(
          config,
          { ...assignment, expires: Date.now() - 1 },
          client,
          state,
          signal,
        ),
      );
      assert.deepEqual(await readdir(join(state, "media")), []);
      const cancel = new AbortController();
      setTimeout(() => cancel.abort(), 100);
      await assert.rejects(
        mediaCommand(
          ffmpeg,
          [
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=32x32:rate=10",
            "-f",
            "null",
            "-",
          ],
          root,
          cancel.signal,
        ),
        /cancelled/,
      );
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
);
