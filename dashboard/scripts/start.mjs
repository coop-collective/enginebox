import { startProdServer } from "vinext/server/prod-server";

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
await startProdServer({ host, port });
