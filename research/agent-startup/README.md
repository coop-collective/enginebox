# Remote agent startup experiment

Manual comparison probes, excluded from product builds. The measured results and
limitations are in [the October 23 report](../../docs/REMOTE-AGENT-TIMING-2026-10-23.md).
These scripts use the installed native CLI or documented provider/controller API.
They do not replace an agent harness or proxy inference.

Measure these boundaries independently:

1. **Admission:** client request to accepted task/session identifier.
2. **Environment ready:** allocation/start through successful guest readiness.
3. **First action:** first observed tool call, when a stream actually exposes it.
4. **Completion:** request through a terminal result and expected answer.

An accepted session is not a ready machine. A controller `running` record is not
the first model token. A resumed conversation is not a resumed VM. Missing data
stays missing. Use the same model, repository revision, environment preparation,
client location and prompt before drawing a provider speed comparison.

## Engine Box

Run Python 3 on the Linux controller host with an existing template, connected
agents, available capacity and permission to run `sudo -n incus` and
`sudo -n enginebox-host lock`. The probe uses the production controller API,
allocates **one new disposable workspace**, and writes a tiny `package.json`
fixture before timing. No project checkout or dependency install is measured.

```sh
python3 research/agent-startup/probe-enginebox.py \
  --run-live --token-file /private/controller/api-token \
  --host node-one --project enginebox \
  --output .state/startup-enginebox.json --trials 3
```

It times fresh Codex/Claude/Cursor conversations, one exact-session follow-up per
working provider, three pause/resumes and three stopped-VM boots. Claude uses the
native `haiku` alias; Codex/Cursor retain their configured defaults. It stops after
the first failed native run for a provider. Finally it verifies the allocation
label, instance UUID and project owner, obtains the host lock, and deletes only
its own stopped VM. Existing workspaces are never selected as fixtures.

On an error, the receipt and fixture are preserved for inspection. Resolve the
cause before using `--resume-allocated` (no native runs yet) or
`--resume-lifecycle` (native runs already terminal) against that same receipt.
If an admitted operation may still be active, inspect its recorded ID first.

## Cursor Cloud

Use Node 22+ and a valid user Cloud Agents API key in `CURSOR_API_KEY`:

```sh
node research/agent-startup/probe-cursor.mjs \
  --run-live --repo https://github.com/your-org/benchmark-repo \
  --ref YOUR_EXACT_COMMIT_SHA --output .state/startup-cursor.json
```

The repository must contain a root `package.json`. The probe performs an auth
preflight, creates one read-only agent with automatic PR creation disabled, and
records admission and observed SSE tool/text/result timings. It archives only
the agent it creates after a terminal result. On stream failure it requests
cancellation of its own run and records whether that request was accepted.
Inspect an ambiguous creation failure before retrying. The first-action numbers
include stream transport/replay delays; they are not server-side timestamps.
The configured account currently fails preflight, so the stream/cleanup path is
provided for a future qualified run and is **not live-validated** in this report.

## Claude Cloud

On macOS/Linux with a trusted benchmark repository and a logged-in official
Claude Code CLI:

```sh
python3 research/agent-startup/probe-claude-submission.py \
  --run-live --repo /path/to/benchmark-repo \
  --output .state/startup-claude --trials 3
```

The installed CLI requires a TTY for `--cloud`; this probe supplies a PTY and
records timestamped native output in private files. Resolve first-use prompts
before measurement; the script never auto-accepts them. It measures **submission
acknowledgement only**. It does not pretend the CLI's successful exit means the
remote task finished. Read-only sessions remain visible in the provider account;
there is no invented archive/status API in this probe.

## Codex Cloud

Inspect the installed official `codex cloud --help`, then `codex cloud` to select
an environment. If that CLI exposes a suitable environment, submit with
`codex cloud exec --env ENV_ID --branch BRANCH 'READ_ONLY_PROMPT'` and monitor
with `codex cloud status TASK_ID`. Record invocation and status observation
timestamps separately. Do not treat an empty legacy CLI environment picker as
evidence that the account has no environments in the newer Codex Cloud product.
The current published-environment workflow must be qualified separately.

Raw receipts, task/session IDs, native transcripts and token files belong in
private ignored `.state/` storage. Only reviewed aggregate results belong in Git.
