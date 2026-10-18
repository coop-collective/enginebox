# Engine Box dashboard

Vinext on Node.js, with a live fleet API, server-side credential handling and operator sessions.

See [setup, architecture and validation](../docs/DASHBOARD.md).

```sh
npm ci
npm run typecheck
npm test
npm run build
npm start
```

Configure the private `.env.local` described in the setup guide before starting. The default bind is `127.0.0.1:3000`. No fleet credentials belong in client environment variables or in Git.
