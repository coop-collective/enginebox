# Release build and self-hosting helpers

Small Node tools used around an Engine Box installation. Deployment itself is your own: stage and switch
releases with the installer (`enginebox-install stage`, `update`, `rollback`; see
[docs/UPDATES.md](../../docs/UPDATES.md)) from whatever pipeline you run privately.

| Script | What it does |
|---|---|
| `src/build.mjs --version X --output DIR` | Builds an isolated, versioned controller release archive and manifest from a clean checkout (workspace versions pinned to `X`). CI uses it for the `controller-preview` artifact. |
| `src/github-secrets.mjs` | Materializes GitHub App secrets for a self-hosted controller ([docs/GITHUB-SECRETS.md](../../docs/GITHUB-SECRETS.md)). |
| `src/onboard-orgs.mjs` | Onboards organizations on a self-hosted controller ([docs/SELF-HOSTING-ORGS.md](../../docs/SELF-HOSTING-ORGS.md)). |

`src/lib.mjs` holds their shared, dependency-free helpers: exact-version checks, artifact selection with
manifest and archive checksums, a quoting shell boundary and a bounded child-process runner that never relays
private child output.

```sh
pnpm --filter @enginebox/deploy test
```
