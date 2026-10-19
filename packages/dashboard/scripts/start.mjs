import { startProdServer } from "vinext/server/prod-server";
import { createConnection, isIP } from "node:net";

const host = process.env.HOST ?? "127.0.0.1";
const port = Number(process.env.PORT ?? 3000);
if (!Number.isInteger(port) || port < 1 || port > 65535)
  throw new Error("PORT must be a valid TCP port.");
if (
  !["127.0.0.1", "localhost", "::1"].includes(host) &&
  !process.env.ENGINEBOX_DASHBOARD_ORIGIN?.startsWith("https://")
) {
  throw new Error(
    "External binding requires an HTTPS ENGINEBOX_DASHBOARD_ORIGIN and a TLS reverse proxy.",
  );
}
if (
  (process.env.ENGINEBOX_DASHBOARD_PASSWORD?.length ?? 0) < 16 ||
  (process.env.ENGINEBOX_SESSION_SECRET?.length ?? 0) < 32 ||
  (process.env.ENGINEBOX_API_TOKEN?.length ?? 0) < 32
) {
  throw new Error(
    "Set ENGINEBOX_DASHBOARD_PASSWORD (16+ characters), ENGINEBOX_SESSION_SECRET (32+ characters), and ENGINEBOX_API_TOKEN (32+ characters).",
  );
}
// A supervisor crash must not leave a detached dashboard occupying a release
// port indefinitely. The authenticated socket is held for the child's lifetime.
if (process.env.ENGINEBOX_SUPERVISOR_LIFELINE) {
  const address = new URL(`tcp://${process.env.ENGINEBOX_SUPERVISOR_LIFELINE}`);
  const hostname = address.hostname.replace(/^\[|\]$/g, "");
  const token = process.env.ENGINEBOX_SUPERVISOR_LIFELINE_TOKEN ?? "";
  if (
    !isIP(hostname) || !["127.0.0.1", "::1"].includes(hostname) ||
    address.username || address.password || address.pathname ||
    address.search || address.hash || !address.port ||
    !/^[!-~]{32,}$/.test(token)
  ) throw new Error("Invalid supervisor lifeline configuration.");
  const socket = createConnection({ host: hostname, port: Number(address.port) });
  await new Promise((resolve, reject) => {
    socket.once("connect", resolve);
    socket.once("error", reject);
  });
  socket.on("error", () => process.exit(1));
  socket.on("close", () => process.exit(0));
  socket.write(`${token}\n`);
}
await startProdServer({ host, port });
