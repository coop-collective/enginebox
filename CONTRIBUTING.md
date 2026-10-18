# Contributing

Engine Box is in early development. Start with the [design](docs/DESIGN.md) and [implementation milestones](docs/IMPLEMENTATION-PLAN.md). The current implementation includes server bootstrap, Incus lifecycle controls, a fleet API/MCP and a Vinext dashboard. Platform qualification and optional CI integration remain open.

```sh
uv sync --project runtime --locked
runtime/.venv/bin/python -m unittest discover -s tests -v
python3 -m unittest discover -s scripts/control-plane -p 'test_*.py' -v
```

For dashboard changes, run `npm ci`, `npm run typecheck`, `npm test`, `npm run build` and `npm run test:e2e` in `dashboard/`. The browser suite requires Playwright Chromium (`npx playwright install chromium`) and uses only test fixture services. Build deployment artifacts on the target platform.

Offline tests must not contact a server. Integration tests must use a disposable VM and explicitly identify the host, project and artifacts they create. Never use a signed-in workspace as an integration-test target.

For a change, explain the behavior, how it was verified and any remaining platform limits. Preserve official provider clients and the optional-module boundary. Add regression coverage for configuration, command handling, lifecycle transitions and host mutations where relevant.

Contributions are licensed under the repository's Apache-2.0 license. Provider binaries, third-party proprietary code and customer workspace recipes do not belong in this repository.
