# Dashboard information architecture

The dashboard is organized around a small object model. This document fixes the
names, what each object means, where it appears and which parts are still
planned. It supersedes the navigation in the first dashboard redesign.

## Object model

Three layers recur across agents, CI and previews:

| Layer | Object | Meaning |
|---|---|---|
| Definition | **Template** | The computer: an image plus how it runs (base image, toolchains, setup, exposed ports, resources). Credential-free and reusable. |
| Job | **Profile**, **CI job**, **Preview** | What runs on a template and with which permissions, connections and secrets. |
| Capacity | **Machine** | A physical computer. It hosts **sandboxes** (agent VMs), CI jobs or engine sessions according to its role. |

Everything a person works on belongs to a **Project**.

| Object | Definition | Today's backend |
|---|---|---|
| Project | The durable project ID. Owns its LayerFS files, branches and accepted main, optional GitHub or hosted Git links, agent runs, previews and storage. | `/v1/workspaces` (named *workspaces* in the API), `/v1/projects/{id}/…` |
| Sandbox | One private agent VM cloned from a sandbox template. An agent run happens in a sandbox. It can pause, hibernate (RAM kept) or stop. | Fleet instances (`workspace_name`), pool members |
| Profile | How and where an agent runs: provider permissions, skills, MCP connections, secret requirements, template, machines and scale. | `/v1/profiles` plus `/v1/pools` (merging planned) |
| Account | A Codex, Claude, Cursor or GLM sign-in, either shared with the organization or personal. | `/v1/agents` (shared) and `/api/credentials` |
| Template | See below. | three registries today |
| Machine | A registered computer with roles: agent host, CI builder, engine. Sandboxes are shown inside the machine that hosts them. | `/v1/compute`, `/api/contributors`, fleet hosts |
| Runner pool | GitHub Actions runner group config. Each job gets a disposable VM. | `/v1/github` |
| Connection | An MCP server or secret source in the organization catalog. Values are write-only. | `/api/catalog/connections` |

"Workspace" is not used as a top-level name any more: it meant both a project and
a VM. The API keeps its existing names.

## Templates

A template is an image plus how to run it. The dashboard shows one Templates page
with a runtime tag. Behind it, three validated formats remain for now:

| Runtime | Today's format | Used by |
|---|---|---|
| VM (sandbox) | Native agent template: Ubuntu 24.04 + native CLIs, resources ([TEMPLATES.md](TEMPLATES.md)) | Profiles, sandboxes |
| Native (build toolchain) | CI toolchain manifest: pinned download, install, verify ([CI.md](CI.md)) | CI jobs on build machines |
| Container (preview runtime) | Dev template in `.enginebox/templates/*.json`: Docker image, prepare, command, ports ([PROJECT-CONFIG.md](PROJECT-CONFIG.md)) | Project previews |

Planned: a single layered manifest (base image, toolchains, setup, ports) that
profiles, CI jobs and previews all reference, and VM sandboxes built from an OCI
image. Until then each format keeps its own validation and lifecycle.

## Navigation

```
Overview
Projects        Project → Agents · Code · Previews · Activity
Agents          Profiles · Accounts
CI              Actions runners
Infrastructure  Machines (with sandboxes) · Templates (Sandbox images · Build toolchains · Preview runtimes)
Settings        Connections & secrets · GitHub · MCP & apps · Organization
```

Old `?panel=` links redirect: `workspaces`, `repositories`, `storage` and
`environments` open Projects; `instances` and `fleet` open Machines; `toolchains`
and `ci` open Templates on Build toolchains; `github` opens Settings → GitHub.

Task workflow (tasks, candidates, approvals) stays in the consuming apps such as
Gamejam; this dashboard does not show it.

## Repository versus dashboard state

Project configuration is GitOps: `.enginebox/config.json` declares profiles,
dev templates and secret *requirements*. Agents edit it like any file.

Planned: the dashboard's visual profile editor writes the file as a LayerFS change
on a branch, and the owner accepts it to main like any other change; GitHub export
follows if enabled.

Only the dashboard holds state that must not be in a repository: secret values,
owner-approved requirement mappings, accounts, machines, connection credentials,
OAuth grants and organization membership.

## Status

| Item | Status |
|---|---|
| Navigation, project tabs, sandboxes inside Machines, unified Templates page | Implemented (UI only) |
| Profiles carry pool placement and scale; pools hidden | Planned (backend) |
| Visual profile editor saving through LayerFS | Planned |
| One account list with shared/personal scope | Partly: both kinds shown on one page |
| Layered template manifest, OCI-built sandboxes | Planned |
| CI build machines listed under Machines | Planned (separate CI inventory today) |
