import { defineConfig } from 'vite';
import vinext from 'vinext';
import { cloudflare } from '@cloudflare/vite-plugin';

const publicHost = (process.env.ENGINEBOX_PUBLIC_HOST || process.env.DEV_PUBLIC_HOST);
export default defineConfig({
  plugins: [vinext(), cloudflare({
    viteEnvironment: { name: 'rsc', childEnvironments: ['ssr'] },
    remoteBindings: false,
    inspectorPort: false,
    persistState: { path: `${process.env.ENGINEBOX_DATA_DIR || '.wrangler'}/state` },
  })],
  cacheDir: '/tmp/vite-cache',
  server: {
    host: '0.0.0.0', port: 8787, strictPort: true,
    allowedHosts: publicHost ? [publicHost] : [],
    ws: publicHost ? { protocol: 'wss', host: publicHost, clientPort: 443 } : undefined,
  },
});
