import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

export default defineConfig({
  // These tests exercise real workerd/SQLite storage. Windows hosted runners
  // need time for the multi-request flows beyond Vitest's five-second default.
  test: { testTimeout: process.platform === "win32" ? 30_000 : 5_000 },
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        bindings: {
          ADMIN_TOKEN: "local-tests-only-0123456789abcdef0123456789",
          ALLOW_HTTP_NODES: "true",
        },
      },
    }),
  ],
});
