# GitHub connections for LayerFS projects

LayerFS owns project files, branches and accepted main revisions. GitHub is an
optional import/export connection. Ordinary workspaces and distributed Store
bundles contain no `.git` directory or Git credentials. Git object caches and
reconciliation environments belong to the connector, outside project storage.
This supersedes the earlier assumption that agents use Git inside LayerFS.

## Transaction boundary

1. Import an exact reviewed GitHub tree into LayerFS, preserving file bytes,
   executable bits and safe symlinks. Record the external commit as provenance,
   not as the project's internal history. Exclude Git configuration and history.
2. Agents fork and checkpoint LayerFS branches. A merge or promotion advances
   LayerFS main through its owning controller. GitHub being unavailable does not
   undo an accepted LayerFS revision.
3. If the project has a connection and outbound sync is enabled, record an outbox
   item naming the exact accepted LayerFS revision, its sequence and its last
   synchronized GitHub commit. Checkpoints on task branches do not create exports.
4. An exclusive sync worker materializes that revision in a dedicated environment
   with Git. Fetch GitHub's current branch and perform a three-way reconciliation
   using the last synchronized files as base. Never merge database files or Store
   SQLite files; reconcile only the explicitly exported source tree.
5. If reconciliation is clean, validate the candidate. If it conflicts, retain
   base/LayerFS/GitHub versions, a conflict list and a task for the specialized
   agent. The agent edits only this environment and receives no GitHub push token.
   No conflict-marker or unresolved-index result can be declared complete.
6. Any resolved tree must be accepted back into LayerFS against the original
   expected main revision before export. If LayerFS main advanced, the candidate
   is stale and must be reconciled again. The export worker, not the agent,
   creates the external commit and conditionally advances GitHub's branch.
7. Compare GitHub's ref with the exact fetched head. If it moved, retain the
   candidate and create a fresh reconciliation attempt. Never overwrite remote
   changes. On a lost push response, observe the remote ref and recognize the
   exact intended commit before retrying; do not mint another commit.

The connector keeps LayerFS revision IDs, Git commit IDs and delivery attempt IDs
as different fields. A commit created externally is not an accepted LayerFS
revision. Failed sync, a pending agent task and successful delivery are separate
visible states. Disabling sync retains pending state and blocks further pushes.

## Conflict task contract

The task contains the project/connection ID, immutable accepted revision,
expected main sequence, last synchronized commit, observed GitHub head,
reconciliation attempt ID, file conflicts and a credential-free Git worktree.
Instructions identify the LayerFS and GitHub sides and require tests appropriate
to the project. The result names the same attempt and a clean resolved tree;
completion never permits an agent to select a different repository or push ref.
Retries use the same attempt until its inputs become stale. A stale attempt is
retained for diagnosis and is never applied to a newer main by guesswork.

The specialized agent runs in an isolated environment containing Git and the
materialized files. This is the explicit exception to Git-free normal workspaces.
Provider authentication follows Engine Box's existing private native-account
contract; repository write credentials remain with the connector. Agent launch
and policy are separate from the deterministic merge/publication state machine,
so tests can simulate an agent without paid inference or real repository writes.

## Distributed ownership

The merged [distributed storage design](STORE-DISTRIBUTION.md) remains
authoritative for fleet ownership: Cloudflare's accepted revision and fencing lease gate exports.
An integration worker consumes an accepted immutable manifest; an arbitrary
worker's local Store head cannot authorize a push. Store distribution and GitHub
sync use separate queues. Project promotion must first satisfy the configured
storage durability gate; a GitHub push is not a backup of the LayerFS Store.

The current local-node adapter uses private durable receipts and file locks.
These are suitable for the existing single-controller owner deployment, not a
replacement for distributed fencing. The publication/outbox interface must be
called by the accepted-main transaction when this connector is integrated with the distributed controller.

## Verification

Tests must cover initial import without Git metadata; executable and binary
content; disabled/no-connection behavior; one export per accepted revision;
nonconflicting upstream edits; edit/edit and delete/edit conflicts; exact
base/ours/theirs attribution; refusal of unresolved agent work; acceptance of
resolved files back into LayerFS; local or remote head changes during resolution;
retries after lost push responses; failed publication preserving the previous
head; and exclusion of credentials from workspaces, receipts and logs.

Git LFS hydration, submodule recursion and repository deletion are separate
features. Unsupported content must be reported rather than pretending pointer
files are the full project. Existing native-Git legacy imports require explicit
migration; their old receipts must not be silently reinterpreted.

## Current implementation and operation

The Rust storage package implements a GitHub-only transport, exact-source review,
Git-free imports, local LayerFS main promotion, a durable reconciliation outbox,
conditional GitHub ref updates and an isolated Docker conflict environment. The
Storage dashboard and controller/MCP expose the same workflow. Begin with
`github-preview`, review and attest, then `github-import` and `github-fork`.
Outbound delivery defaults off. Observe `github-settings` before publishing so
`github-promote` carries the exact expected main revision.

Git is confined to the connector's private fetch cache and per-attempt environment.
The agent container receives only the reconciliation worktree, a read-only image,
private temporary home, bounded CPU/RAM/processes and no Docker socket or push
credential. The executor assigns the native provider account and runs its
specialized task. Stop that container before requesting `resolve`. The connector
checks Git index resolution, unstaged/untracked changes, tracked secret paths,
symlink containment and conflict markers on conflicted files. It discards agent
Git configuration and refuses object-directory indirection before invoking Git
on the host. Agent test execution is part of the task contract; a clean Git index
alone is not a passing application test suite.

The source export is an immutable accepted-main snapshot, not an arbitrary live
mount. Git ignore rules govern new untracked source additions, while previously
tracked files remain tracked. Applying a resolved source removes only source paths
that were deleted; ignored dependency caches and app data stay in LayerFS. A new
GitHub source path colliding with retained data is refused, including after a
retry. Source application refuses symlink parents and never replaces a directory
containing retained data. Known private paths are refused before they enter the
sync environment. Projects that mix credentials or generated private runtime
data into the source root need a separate source export before syncing; this
connector does not merge live application databases or infer a safe data policy.

Local Git fixtures exercise the real three-way index and object operations.
Controller/MCP tests validate requests and credential boundaries; browser fixtures
exercise the UI contract. They do not qualify a deployed GitHub push, Linux
LayerFS SDK publication/export, or paid native-agent inference in the conflict
container. Those require a disposable Linux environment and test repository.
The distributed accepted-manifest/outbox hook is not yet wired to this connector.
The merged Cloudflare fleet implementation does not by itself migrate the live
Rust controller or qualify distributed GitHub delivery.

Protocol references: [Git fetch](https://git-scm.com/docs/git-fetch),
[conditional pushes](https://git-scm.com/docs/git-push), and the pinned
[LayerFS branch API](https://github.com/Ephemeral-AI-Lab/layerfs/blob/968a5d0be0ea3e9456b06f9148b65523138e9490/sdk/js/src/index.js).

The October 24 disposable live-app qualification exercised a private GitHub import,
Git-free LayerFS edits, application tests/build, closed-checkpoint preview, main
promotion and a real conditional GitHub push. The resulting commit changed only
the intended source file; ignored build output stayed out of GitHub. The main dev
service followed LayerFS and retained its separate application data. This closes
the clean-path Linux publication/export and GitHub-write qualification for that
single-owner fixture. Live conflict-agent dispatch and distributed delivery remain
separate gates. The test also caught and corrected empty exports from Docker's
archive API when `/project` is a later-installed LayerFS mount.
