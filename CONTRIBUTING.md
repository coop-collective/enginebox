# Contributing

Engine Box is in early development. Start with the [design](docs/DESIGN.md) and [implementation milestones](docs/IMPLEMENTATION-PLAN.md). The current runtime manages prepared Incus guests; the portable installer is still planned.

```sh
uv sync --project runtime --locked
runtime/.venv/bin/python -m unittest discover -s tests -v
```

Offline tests must not contact a server. Integration tests must use a disposable VM and explicitly identify the host, project and artifacts they create. Never use a signed-in workspace as an integration-test target.

For a change, explain the behavior, how it was verified and any remaining platform limits. Preserve official provider clients and the optional-module boundary. Add regression coverage for configuration, command handling, lifecycle transitions and host mutations where relevant.

Contributions are licensed under the repository's Apache-2.0 license. Provider binaries, third-party proprietary code and customer workspace recipes do not belong in this repository.
