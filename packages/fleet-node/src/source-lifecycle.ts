import { spawn } from "node:child_process";
import { isAbsolute } from "node:path";
import { requireThat } from "@enginebox/fleet-protocol";

// Installed by the operator, outside project source. No browser or model may
// select this executable or its arguments. It uses the existing storage owner.
export type SourceLifecycle = { command: string[]; timeoutSeconds?: number };
export class SourceLifecycleError extends Error {
  uncertain: boolean;
  constructor(message: string, uncertain: boolean) {
    super(message); this.uncertain = uncertain;
  }
}
export function validateLifecycle(value: SourceLifecycle) {
  requireThat(Array.isArray(value.command) && value.command.length > 0 && value.command.length <= 12 && isAbsolute(value.command[0]) && value.command.every(a => typeof a === "string" && a.length <= 1024 && !/[\x00-\x1f]/.test(a)), "Invalid operator source lifecycle command");
  requireThat(Number.isInteger(value.timeoutSeconds ?? 300) && (value.timeoutSeconds ?? 300) >= 1 && (value.timeoutSeconds ?? 300) <= 900, "Invalid source lifecycle timeout");
}
export async function sourceAction(config: SourceLifecycle, action: "prepare" | "complete", project: string, attempt: string, signal: AbortSignal): Promise<string> {
  validateLifecycle(config);
  if (signal.aborted) throw new SourceLifecycleError("Source operation lease expired", true);
  return new Promise((resolve, reject) => {
    const child = spawn(config.command[0], config.command.slice(1), { stdio: ["pipe", "pipe", "ignore"], env: { PATH: "/usr/local/bin:/usr/bin:/bin" }, detached: true });
    let output = Buffer.alloc(0), stopped = false;
    const stop = () => { stopped = true; if (child.pid) { try { process.kill(-child.pid, "SIGKILL"); } catch {} } };
    const timer = setTimeout(stop, (config.timeoutSeconds ?? 300) * 1000);
    signal.addEventListener("abort", stop, { once: true });
    child.stdout.on("data", chunk => { if (output.length + chunk.length > 65536) stop(); else output = Buffer.concat([output, chunk]); });
    child.on("error", () => { clearTimeout(timer); signal.removeEventListener("abort", stop); reject(new SourceLifecycleError("The operator source service could not be started", true)); });
    child.on("close", code => {
      clearTimeout(timer); signal.removeEventListener("abort", stop);
      if (stopped || code !== 0) { reject(new SourceLifecycleError("Source operation interrupted; owner reconciliation is required", true)); return; }
      try {
        const result = JSON.parse(output.toString());
        requireThat(typeof result.ok === "boolean" && typeof result.summary === "string" && result.summary.length <= 4000, "Invalid source receipt");
        if (!result.ok) reject(new SourceLifecycleError(result.summary, result.uncertain !== false));
        else resolve(result.summary);
      } catch { reject(new SourceLifecycleError("Source service returned no valid receipt", true)); }
    });
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify({ action, project, attempt }));
  });
}
