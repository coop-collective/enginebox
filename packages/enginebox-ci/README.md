# Engine Box CI

Optional non-agent build execution and GitHub Actions support. See
[setup, trigger format, dedicated node policy and qualification limits](../../docs/CI.md).

The core runtime remains independent of this crate. The controller uses its
existing durable job queue and ownership/drain protocol; the `ebox ci` CLI and
operator MCP expose registration, planning and triggering. Native OS workers run
shell automations or the official ephemeral GitHub Actions runner.

```sh
cargo test -p enginebox-ci -p enginebox-controller -p enginebox-cli --locked
actionlint examples/ci/workflows/*.yml
```
