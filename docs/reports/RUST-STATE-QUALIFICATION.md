# Rust controller metadata qualification

The opt-in controller integration test passed against the existing Python
controller's persisted metadata on 2026-10-20. Rust loaded 33 completed jobs,
6 completed native agent runs, and 5 template records, including 1 current
template. `Controller::new`, activation, and draining succeeded in a disposable
private copy. Original file contents and identities remained unchanged. No
backward-compatibility change was required for these records.

This is a persisted-metadata compatibility check. It does not switch the deployed
controller, authenticate a provider, resume an agent session, or qualify live VM
operations. The active service was left running. Native-agent qualification and
the staged controller cutover remain separate checks.

## What the test does

The test in
[`legacy_state_qualification.rs`](../../packages/enginebox-controller/tests/legacy_state_qualification.rs)
reads only `jobs`, `agent-runs`, and `templates` below an explicitly supplied
source state directory, plus an explicitly supplied fleet configuration file.
It copies the completed metadata and configuration into a new mode-0700
temporary directory, with mode-0600 files. Template lock files are observed for
source stability but never copied. Provider homes, authentication state, refresh
credentials, controller ownership locks, and workspace state are excluded.

It refuses source symlinks, multiply linked files, unexpected directory layouts,
nonprivate metadata, incomplete or active records, and uncertain agent runs.
File and total-copy bounds keep the operation small. Source bytes and file
identity metadata are checked before controller activation and again afterward,
including when copied metadata fails validation. Reading may update filesystem
access times; the test does not write source files.

The copied controller never starts its background loop, an HTTP listener, or an
API operation. It validates current and historical templates, activates, drains,
and verifies that copied terminal records were not rewritten and generated
provider directories contain no files. A Linux x86-64 seccomp filter on the test
thread denies socket creation, connections, process execution, and child/thread
creation before reading source data. A separate fixture checks that the filter
actually rejects socket and process creation.

Output contains only counts and pass flags. Errors contain static phase names;
configuration, prompts, native output, and record contents are never printed.
The temporary copy is removed when the test finishes.

## Run the check

Build on a Linux x86-64 machine with the repository's pinned Rust toolchain:

```sh
cargo test -p enginebox-controller --test legacy_state_qualification --locked --no-run
```

Use the test executable path printed by that command. Resolve an administrator's
release alias to the real configuration path first: the test rejects symlinks in
either source path. Run as an identity that can read the private source metadata.
The paths below are placeholders, not deployment defaults:

```sh
ENGINEBOX_QUALIFY_LEGACY_STATE=1 \
ENGINEBOX_LEGACY_STATE_SOURCE=/absolute/private/controller-state \
ENGINEBOX_LEGACY_FLEET_CONFIG=/absolute/real/release/etc/fleet.toml \
/absolute/path/to/legacy_state_qualification-test-binary \
  --ignored --exact qualify_actual_legacy_state_without_external_calls \
  --nocapture --test-threads=1
```

Do not point the controller itself at the live state directory for this check.
The test alone reads the supplied source paths and chooses its own disposable
destination. Do not copy provider directories to make the check pass. If the
source changes during capture, wait for a stable interval and repeat; a failed
copy is not evidence that the deployed data is incompatible.

The successful Linux qualification reported:

```json
{
  "passed": true,
  "jobs": 33,
  "agent_runs": 6,
  "template_records": 5,
  "current_templates": 1,
  "source_unchanged": true,
  "provider_state_copied": false,
  "background_started": false,
  "host_or_provider_api_calls": 0
}
```

The automated fixtures also cover active/uncertain record rejection, exclusion
of credentials and lock files, symlink refusal, successful copied-controller
activation/draining, and a strict agent-record decode failure that leaves the
source unchanged. The opt-in live-source test stays ignored during normal test
runs.
