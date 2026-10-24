# Engine Box licensing

Current and later Engine Box versions are licensed under the **Business Source License 1.1** (BSL). The complete, unmodified terms and the Engine Box parameters are in [LICENSE](../LICENSE). This is a source-available license, not an OSI open-source license. The license text governs; this page explains how the project applies it.

In short: personal, non-commercial and non-production use is free. Production use by or on behalf of a business needs a paid license of **$10 per node per month**. Offering Engine Box to third parties as a hosted or managed service needs a separate agreement. Each version converts to Apache-2.0 four years after its first public release.

## What each use costs

| Use | Terms |
|---|---|
| Personal use on your own machines | Free. |
| Non-commercial use, such as a hobby project, study, research or a non-profit's own non-commercial work | Free. |
| Testing, evaluation, development and other non-production use, including by a business | Free (BSL non-production grant). |
| Production use by a business, or on its behalf, on machines it controls | $10 per node per month. See [what counts as a node](#what-counts-as-a-node). |
| Modifying the software for any of the uses above | Allowed. Modified copies are covered by the same license. |
| Offering Engine Box workspaces, dashboard, API or MCP to third parties as a hosted or managed service | Not allowed under the free grant or the per-node license. Requires a separate agreement. |
| Building independent software without using Engine Box code | Not controlled by the Engine Box license. |

These examples explain the intended boundary. If your situation is unclear, ask before relying on the free grant. This policy does not grant rights to provider subscriptions, third-party packages, operating systems, game engines, customer code or trademarks.

## What counts as a node

A **node** is each machine registered with an Engine Box controller to run workspaces. Count a machine once, however many workspaces, templates or users it hosts. A physical host and each virtual machine registered as its own node each count as one.

The controller itself does not count unless it also runs workspaces, that is, unless it is also registered as a node. A machine used only to operate the dashboard, API or MCP server is free of node charges.

Example: a controller on one small server plus three Linux hosts registered to run workspaces is 3 nodes, or $30 per month. If the controller server is also registered to run workspaces, it is 4 nodes, or $40 per month.

The software does not currently enforce node counts. The license is the agreement, and you are responsible for licensing the nodes you run in production.

## How to buy

Licenses are bought from the [pricing page](https://xavalabs.com/pricing/). Checkout is not open yet; the page says so until it is. Provider subscriptions (Codex, Claude, Cursor) are separate and stay with the provider.

## Change Date

Each version has its own Change Date: four years after that version is first publicly released. On that date the licensor grants rights under the **Apache License, Version 2.0** for that version, and the BSL terms for it end. Later versions keep their own dates.

## History

Three licenses have applied over time:

1. **Apache-2.0**: code through commit `19f834a344d91bb1e6aa1cc46841ad42eea830ee`, including the then-public development branch. Those grants remain in effect and cannot be revoked by a later change. See [LICENSE-APACHE-2.0](../LICENSE-APACHE-2.0).
2. **Sustainable Use License 1.0**: versions after that commit, up to the change to the BSL. The unmodified text, originally published by [n8n](https://github.com/n8n-io/n8n/blob/master/LICENSE.md), is preserved in [LICENSE-SUSTAINABLE-USE-1.0](../LICENSE-SUSTAINABLE-USE-1.0). The grants for those versions remain.
3. **Business Source License 1.1**: current and later versions, as described above.

The [NOTICE](../NOTICE) file records the same history. Anyone may keep using an earlier version under the license it was published with.

## Contributions and packaging

Contributions are accepted under the repository license. The project needs the right to offer commercial licenses, and a contributor license agreement is planned. See [CONTRIBUTING.md](../CONTRIBUTING.md).

Runtime and dashboard packages carry LICENSE, NOTICE and the earlier licenses. Cargo metadata points to LICENSE and npm metadata uses `BUSL-1.1`. The release builder includes LICENSE, NOTICE, LICENSE-APACHE-2.0 and LICENSE-SUSTAINABLE-USE-1.0. Third-party licenses remain separate.

## Open items

The licensor named in LICENSE is a placeholder until the legal entity that sells licenses is chosen.
