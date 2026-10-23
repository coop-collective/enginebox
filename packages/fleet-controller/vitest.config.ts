import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

export default defineConfig({
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
