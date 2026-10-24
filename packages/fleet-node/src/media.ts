import { spawn } from "node:child_process";
import { mkdir, mkdtemp, open, rm, stat, writeFile } from "node:fs/promises";
import { join, isAbsolute } from "node:path";
import { createHash } from "node:crypto";
import {
  CHUNK_BYTES,
  readBounded,
  RECORDING_LIMITS,
  requireThat,
  sha256,
  type Assignment,
  type Recording,
  type RecordingMedia,
} from "@enginebox/fleet-protocol";
import { Client } from "./client.ts";
import { UncertainNativeRun } from "./native-assistant.ts";

export type MediaConfig = {
  ffmpeg: string;
  ffprobe: string;
  projects: string[];
};
type Probe = {
  format?: { duration?: string };
  streams?: {
    codec_type?: string;
    width?: number;
    height?: number;
    channels?: number;
    sample_rate?: string;
  }[];
};

// Only fixed FFmpeg commands are used. There is no shell, remote input URL,
// project executable, or caller-provided command line in this executor.
export async function mediaCommand(
  binary: string,
  args: string[],
  directory: string,
  signal: AbortSignal,
  timeoutMs = 120_000,
): Promise<string> {
  requireThat(
    isAbsolute(binary),
    "Media tools must have owner-configured absolute paths",
  );
  requireThat(
    process.platform !== "win32",
    "Run media validation in the Linux development container",
  );
  signal.throwIfAborted();
  return new Promise((resolve, reject) => {
    const child = spawn(binary, args, {
      cwd: directory,
      detached: true,
      windowsHide: true,
      stdio: ["ignore", "pipe", "pipe"],
      env: { PATH: "/usr/bin:/bin", HOME: directory, LANG: "C" },
    });
    let output = "",
      errors = "",
      stopped = false,
      killTimer: ReturnType<typeof setTimeout> | undefined,
      settleTimer: ReturnType<typeof setTimeout> | undefined,
      outputBytes = 0,
      settled = false,
      killFailed = false;
    function kill(signal: NodeJS.Signals) {
      if (child.pid)
        try {
          process.kill(-child.pid, signal);
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ESRCH") {
            errors += "; process termination failed";
            killFailed = true;
          }
        }
    }
    function stop() {
      if (stopped) return;
      stopped = true;
      kill("SIGTERM");
      killTimer = setTimeout(() => kill("SIGKILL"), 2000);
      settleTimer = setTimeout(() => {
        if (!settled) {
          settled = true;
          cleanup();
          reject(
            new UncertainNativeRun(
              `Media process group ${child.pid} did not confirm cancellation; retain its reservation`,
            ),
          );
        }
      }, 4000);
    }
    const timer = setTimeout(stop, timeoutMs);
    signal.addEventListener("abort", stop, { once: true });
    child.stdout.on("data", (data) => {
      const available = Math.max(0, 1024 * 1024 - outputBytes);
      output += data.subarray(0, available).toString();
      outputBytes += Math.min(available, data.length);
      if (data.length > available) stop();
    });
    child.stderr.on("data", (data) => {
      errors = (errors + data.toString()).slice(-16 * 1024);
    });
    const cleanup = () => {
      clearTimeout(timer);
      if (killTimer) clearTimeout(killTimer);
      if (settleTimer) clearTimeout(settleTimer);
      signal.removeEventListener("abort", stop);
    };
    child.once("error", (error) => {
      cleanup();
      if (settled) return;
      settled = true;
      reject(error);
    });
    child.once("close", (code) => {
      // A parent can exit before descendants that ignore SIGTERM. Finish the
      // owned process group before clearing its escalation timer.
      kill("SIGKILL");
      cleanup();
      if (settled) return;
      settled = true;
      if (killFailed) {
        reject(
          new UncertainNativeRun(
            `Media process group ${child.pid} could not be confirmed stopped`,
          ),
        );
        return;
      }
      if (code !== 0 || stopped || signal.aborted)
        reject(
          new Error(
            `Media validation failed (${code}): ${stopped ? "cancelled or timed out" : errors.slice(-2000)}`,
          ),
        );
      else resolve(output);
    });
  });
}

export async function executeMedia(
  config: MediaConfig,
  assignment: Assignment,
  client: Client,
  state: string,
  outerSignal: AbortSignal,
): Promise<Record<string, unknown>> {
  requireThat(
    assignment.kind === "media-finalize" &&
      config.projects.includes(assignment.project),
    "Media executor is not configured for this project",
    409,
  );
  const cancel = new AbortController(),
    onAbort = () => cancel.abort();
  outerSignal.addEventListener("abort", onAbort, { once: true });
  if (outerSignal.aborted) cancel.abort();
  const deadline = Date.now() + assignment.timeoutSeconds * 1000;
  const watchdog = setInterval(() => {
    if (assignment.expires <= Date.now() || Date.now() >= deadline)
      cancel.abort();
  }, 100);
  const signal = cancel.signal;
  let directory: string | undefined,
    uncertain = false;
  try {
    const base = join(state, "media");
    await mkdir(base, { recursive: true, mode: 0o700 });
    directory = await mkdtemp(join(base, "attempt-"));
    const prefix = `/v1/projects/${assignment.project}/recordings/${assignment.recording}`;
    const formats = [
      "-protocol_whitelist",
      "file,pipe",
      "-format_whitelist",
      "matroska,webm,mov,mp4,m4a,3gp,3g2,mj2",
    ];
    const common = [
      "-hide_banner",
      "-nostdin",
      "-loglevel",
      "error",
      "-max_alloc",
      "67108864",
    ];
    const command = (args: string[]) =>
      mediaCommand(
        config.ffmpeg,
        [...common, ...args],
        directory!,
        signal,
        Math.min(120_000, Math.max(1, deadline - Date.now())),
      );
    signal.throwIfAborted();
    const recordingResponse = await client.request(prefix, { signal });
    const recording = JSON.parse(
      new TextDecoder().decode(
        await readBounded(recordingResponse.body, 2 * 1024 * 1024),
      ),
    ) as Recording;
    requireThat(
      recording.state === "finalizing" &&
        recording.generation === assignment.generation &&
        recording.finish &&
        recording.bytes <= RECORDING_LIMITS.maxBytes,
      "Recording no longer matches this job",
      409,
    );
    let downloaded = 0;
    const extension = recording.mimeType.startsWith("video/mp4")
      ? "mp4"
      : "webm";
    const files: string[] = [];
    for (const segment of recording.finish.segments) {
      const file = `segment-${segment.index}.${extension}`,
        path = join(directory, file);
      const handle = await open(path, "wx", 0o600);
      try {
        for (const sequence of segment.chunks) {
          signal.throwIfAborted();
          const chunk = recording.chunks.find(
            (chunk) => chunk.sequence === sequence,
          );
          requireThat(
            chunk?.uploaded && chunk.segment === segment.index,
            "Recording segment is incomplete",
            409,
          );
          const response = await client.request(
            `${prefix}/chunks/${sequence}`,
            { signal },
          );
          const bytes = await readBounded(
            response.body,
            RECORDING_LIMITS.maxChunkBytes,
          );
          requireThat(
            bytes.length === chunk.bytes &&
              (await sha256(bytes)) === chunk.sha256,
            "Recording input checksum differs",
            409,
          );
          downloaded += bytes.length;
          requireThat(
            downloaded <= RECORDING_LIMITS.maxBytes,
            "Recording exceeds byte limit",
          );
          await handle.writeFile(bytes);
        }
        await handle.sync();
      } finally {
        await handle.close();
      }
      // Probe only local, explicitly supported containers before decoding.
      const probe = JSON.parse(
        await mediaCommand(
          config.ffprobe,
          [
            "-v",
            "error",
            ...formats,
            "-max_alloc",
            "67108864",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
            file,
          ],
          directory,
          signal,
        ),
      ) as Probe;
      const video =
        probe.streams?.filter((stream) => stream.codec_type === "video") ?? [];
      requireThat(
        video.length === 1 &&
          video[0].width! > 0 &&
          video[0].height! > 0 &&
          video[0].width! <= 3840 &&
          video[0].height! <= 2160,
        "Recording needs one bounded video stream",
      );
      requireThat(
        (probe.streams ?? []).every(
          (stream) =>
            stream.codec_type === "video" ||
            (stream.codec_type === "audio" &&
              (stream.channels ?? 0) <= 2 &&
              Number(stream.sample_rate) <= 96000),
        ),
        "Unsupported recording streams",
      );
      const duration = Number(probe.format?.duration);
      requireThat(
        !Number.isFinite(duration) || duration <= 610,
        "Recording exceeds duration limit",
      );
      files.push(file);
    }
    requireThat(
      downloaded === recording.bytes,
      "Recording input byte count differs",
    );
    let input: string[];
    if (files.length === 1) input = [...formats, "-i", files[0]];
    else {
      await writeFile(
        join(directory, "segments.txt"),
        files.map((file) => `file '${file}'`).join("\n"),
        { mode: 0o600, flag: "wx" },
      );
      const concatFormats = [...formats];
      concatFormats[3] += ",concat";
      input = [
        ...concatFormats,
        "-f",
        "concat",
        "-safe",
        "1",
        "-i",
        "segments.txt",
      ];
    }
    const output = `recording.${extension}`;
    // Remux instead of re-encoding; this creates a seekable index at low CPU cost.
    await command([
      ...input,
      "-map",
      "0:v:0",
      "-map",
      "0:a:0?",
      "-map_metadata",
      "-1",
      "-c",
      "copy",
      "-avoid_negative_ts",
      "make_zero",
      ...(extension === "mp4" ? ["-movflags", "+faststart"] : []),
      "-fs",
      String(RECORDING_LIMITS.maxBytes + 16 * 1024 * 1024),
      output,
    ]);
    const probe = JSON.parse(
      await mediaCommand(
        config.ffprobe,
        [
          "-v",
          "error",
          ...formats,
          "-max_alloc",
          "67108864",
          "-show_streams",
          "-show_format",
          "-of",
          "json",
          output,
        ],
        directory,
        signal,
      ),
    ) as Probe;
    const video = probe.streams?.find(
        (stream) => stream.codec_type === "video",
      ),
      durationMs = Math.round(Number(probe.format?.duration) * 1000);
    requireThat(
      video &&
        Number.isSafeInteger(durationMs) &&
        durationMs > 0 &&
        durationMs <= 610_000,
      "Decoded recording duration is invalid",
    );
    // A successful mux alone does not establish playable media. Decode all frames.
    await command([
      "-xerror",
      "-err_detect",
      "explode",
      ...formats,
      "-threads",
      "2",
      "-i",
      output,
      "-map",
      "0:v:0",
      "-map",
      "0:a:0?",
      "-f",
      "null",
      "-",
    ]);
    const path = join(directory, output),
      bytes = (await stat(path)).size;
    requireThat(
      bytes > 0 && bytes <= RECORDING_LIMITS.maxBytes + 16 * 1024 * 1024,
      "Decoded media exceeds byte limit",
    );
    const file = await open(path, "r"),
      hash = createHash("sha256"),
      chunks: RecordingMedia["chunks"] = [];
    try {
      let position = 0;
      while (position < bytes) {
        signal.throwIfAborted();
        const buffer = Buffer.alloc(Math.min(CHUNK_BYTES, bytes - position));
        let read = 0;
        while (read < buffer.length) {
          const part = await file.read(
            buffer,
            read,
            buffer.length - read,
            position + read,
          );
          requireThat(part.bytesRead > 0, "Decoded media was truncated");
          read += part.bytesRead;
        }
        hash.update(buffer);
        const digest = createHash("sha256").update(buffer).digest("hex"),
          sequence = chunks.length;
        await client.request(`${prefix}/output/${sequence}`, {
          method: "PUT",
          body: buffer,
          headers: {
            "x-chunk-sha256": digest,
            "x-enginebox-attempt": assignment.attempt,
          },
          signal,
        });
        chunks.push({ sequence, sha256: digest, bytes: buffer.length });
        position += buffer.length;
      }
    } finally {
      await file.close();
    }
    signal.throwIfAborted();
    const media: RecordingMedia = {
      sha256: hash.digest("hex"),
      bytes,
      chunks,
      durationMs,
      width: video.width!,
      height: video.height!,
      hasAudio:
        probe.streams?.some((stream) => stream.codec_type === "audio") ?? false,
      mimeType: extension === "webm" ? "video/webm" : "video/mp4",
    };
    return { recording: recording.id, media };
  } catch (error) {
    uncertain = error instanceof UncertainNativeRun;
    throw error;
  } finally {
    clearInterval(watchdog);
    outerSignal.removeEventListener("abort", onAbort);
    if (directory && !uncertain)
      await rm(directory, { recursive: true, force: true });
  }
}
