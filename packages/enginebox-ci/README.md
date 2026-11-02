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

## Preconnected standbys

An explicitly configured controller standby launches a new disposable worker
before demand. The request uses `reserve: true`, `job_id: 0`, and a separate
`idle_timeout_seconds` (60–21600). Ordinary requests require a positive job ID
and omit the idle timeout. `timeout_seconds` remains the per-job limit: the
official runner's job-start hook begins that deadline, even when assignment
arrives after the idle allowance while retirement is pending. A protected preparation hook is
preserved and runs after the local assignment marker.

A standby is a private, running, one-job VM with JIT credentials, counted in the
same host and controller capacity as demand workers. It is never paused into a
reusable image, returned to a reserve, or used as a cache source. Prepared
workers attach a new private LayerFS branch before boot through the existing
repository-bound admission. Generic standbys keep the ordinary native runner
security model. The controller must restrict the standby's runner group to its
approved private repository and workflows before issuing JIT.

`enginebox-ci-worker --config /absolute/revisioned-worker.json
--retire-reserve ebox-ci-<uuid>` records durable retirement intent. During boot,
it prevents the eventual listener from starting. For an already connected
standby, the controller must first retire the exact GitHub registration after
checking identity and nonbusy state. The native operation then records intent
and waits for natural listener exit; it never signals an idle/retiring listener.
A job accepted before its start hook still receives its full per-job budget.
An assigned job returns `busy` and continues. `requested` is not cleanup proof:
the caller retries until the worker finishes and proves exact VM deletion.

The idle deadline requests controller reconciliation; it is not a destructive
wall-clock deadline. A controller outage or an ambiguous registration retains
the private VM and its capacity until safe reconciliation. `reserve_expired`
never claims that no job was assigned or that a job succeeded. Native signalling
is reserved for an assigned job exceeding its own deadline. SIGTERM is not a
safe idle-drain fallback: official runner 2.338.2 cancels active jobs on listener
shutdown. See [Runner.cs](https://github.com/actions/runner/blob/v2.338.2/src/Runner.Listener/Runner.cs)
and [JobDispatcher.cs](https://github.com/actions/runner/blob/v2.338.2/src/Runner.Listener/JobDispatcher.cs).

Receipts retain template/snapshot identity, resources, and millisecond timestamps
for allocation, preparation, boot, runner launch, actual hook start, verifier
approval, listener exit, and VM destruction. Template or worker changes use a
new immutable worker configuration path in controller desired state, so old
standbys retire before replacement. Do not edit an active worker configuration
in place or promote any standby disk into a template.
