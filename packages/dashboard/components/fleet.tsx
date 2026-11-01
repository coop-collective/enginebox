"use client";

import { FormEvent, useEffect, useState } from "react";
import type { Allocation, Fleet, Host, Instance, Job, Resources } from "@/lib/types";
import { coldSlots, matchingPools, validResources } from "@/lib/resources";
import { Icon, Meter } from "@/components/ui";

const GiB = 1024 ** 3;
export const activeJob = (job: Job) => job.status === "queued" || job.status === "running";
export const poolReserve = (instance: Instance) =>
  !!instance.warm_pool_state && instance.warm_pool_state !== "claimed";
export const bytes = (value?: number) =>
  typeof value === "number" && Number.isFinite(value)
    ? `${(value / GiB).toLocaleString(undefined, { maximumFractionDigits: 1 })} GiB`
    : "—";
export const title = (value: string) => value.charAt(0).toUpperCase() + value.slice(1);

export function Status({ status, stateful }: { status: string; stateful?: boolean }) {
  const label =
    status === "Stopped" && stateful
      ? "Hibernated"
      : status === "Frozen"
        ? "Paused"
        : status;
  const key = status.toLowerCase();
  const tone = ["online", "succeeded", "ready"].includes(key) || key === "running"
    ? "ok"
    : ["queued", "starting", "preparing", "syncing"].includes(key)
      ? "agent"
      : ["offline", "failed", "error", "quarantined"].includes(key)
        ? "danger"
        : "neutral";
  return (
    <span className={`tag status status-${key}`} data-tone={tone}>
      <i className="tag-dot" aria-hidden="true" />
      {label}
    </span>
  );
}

export function HostCard({ host }: { host: Host }) {
  const c = host.capacity;
  return (
    <article className={`host-card ${!host.online ? "offline" : ""}`}>
      <div className="host-heading">
        <div className="host-symbol">
          <Icon name="server" size={18} />
        </div>
        <div>
          <h3>Host resources</h3>
          <span className="mono subtle">{host.id}</span>
        </div>
        <span className="subtle">{host.online ? "Inventory available" : "Inventory unavailable"}</span>
      </div>
      {!host.online || !c ? (
        <p className="host-error" role="status">
          {host.error ??
            "This host is not reachable. Check the connection and refresh."}
        </p>
      ) : (
        <>
          <div className="resource-line">
            <span>CPU allocated</span>
            <strong>
              {c.allocated_cpu} <small>/ {c.cpu_total} cores</small>
            </strong>
          </div>
          <Meter used={c.allocated_cpu} total={c.cpu_total} />
          <div className="resource-line">
            <span>Memory allocated</span>
            <strong>
              {bytes(c.allocated_memory_bytes)}{" "}
              <small>/ {bytes(c.memory_total_bytes)}</small>
            </strong>
          </div>
          <Meter used={c.allocated_memory_bytes} total={c.memory_total_bytes} />
          <div className="host-details">
            <span>
              Available memory<strong>{bytes(c.memory_available_bytes)}</strong>
            </span>
            <span>
              Free disk<strong>{bytes(c.disk_free_bytes)}</strong>
            </span>
            {typeof c.disk_budget_available_bytes === "number" && (
              <span>
                Workspace disk budget
                <strong>{bytes(c.disk_budget_available_bytes)}</strong>
              </span>
            )}
            {typeof c.contribution_cpu === "number" && (
              <span>
                Fleet CPU limit
                <strong>{c.contribution_allocated_cpu ?? 0} / {c.contribution_cpu} cores</strong>
              </span>
            )}
            {typeof c.contribution_memory_bytes === "number" && (
              <span>
                Fleet memory limit
                <strong>{bytes(c.contribution_allocated_memory_bytes)} / {bytes(c.contribution_memory_bytes)}</strong>
              </span>
            )}
          </div>
          <div className="host-foot">
            <span>
              {c.reserve_cpu} cores · {bytes(c.reserve_memory_bytes)} reserved
            </span>
            <span className="capacity-badge">
              {c.max_additional.standard ?? 0} standard cold slots
            </span>
          </div>
          {!!host.warm_pools?.length && (
            <div
              className="pool-summary"
              aria-label="Prepared workspace reserve"
            >
              <h4>Prepared reserve</h4>
              {host.warm_pools.map((pool) => (
                <div
                  className="pool-summary-item"
                  key={`${pool.template}/${pool.size}`}
                >
                  <strong>
                    {pool.template} · {title(pool.size)}
                  </strong>
                  {pool.resources && (
                    <span>
                      {pool.resources.cpu} vCPU · {pool.resources.memory_gib}{" "}
                      GiB RAM · {pool.resources.disk_gib} GiB disk
                    </span>
                  )}
                  <span>
                    {pool.ready} ready · {pool.preparing} preparing · target{" "}
                    {pool.target}
                  </span>
                  {pool.error && <p className="pool-error">{pool.error}</p>}
                </div>
              ))}
              <p>Ready workspaces are paused and keep memory allocated.</p>
            </div>
          )}
        </>
      )}
    </article>
  );
}

export function WorkspaceRow({
  host,
  instance,
  busy,
  action,
  connect,
}: {
  host: Host;
  instance: Instance;
  busy: boolean;
  action: (host: string, name: string, action: string) => void;
  connect: () => void;
}) {
  const running = instance.status === "Running";
  const resumable = ["Stopped", "Frozen"].includes(instance.status);
  const reserved = poolReserve(instance);
  return (
    <tr className={reserved ? "pool-reserve-row" : undefined}>
      <td>
        <div className="workspace-name">
          <span className="workspace-icon">
            <Icon name="terminal" size={18} />
          </span>
          <div>
            <strong>{instance.workspace_name || instance.name}</strong>
            {instance.workspace_name &&
              instance.workspace_name !== instance.name && (
                <span className="instance-id">VM: {instance.name}</span>
              )}
            <span>{instance.ipv4?.join(", ") || "No address reported"}</span>
          </div>
        </div>
      </td>
      <td>
        <span className="host-name">{host.label}</span>
      </td>
      <td>
        <Status status={instance.status} stateful={instance.stateful} />
        {reserved && (
          <span className={`pool-state pool-state-${instance.warm_pool_state}`}>
            Pool reserve · {title(instance.warm_pool_state!)}
          </span>
        )}
      </td>
      <td>
        <strong className="allocation">
          {instance.cpu ?? "—"} vCPU · {bytes(instance.memory_bytes)}
        </strong>
        <span className="table-detail">
          {typeof instance.memory_usage_bytes === "number"
            ? `${bytes(instance.memory_usage_bytes)} memory used`
            : "Usage unavailable"}
        </span>
      </td>
      <td className="actions">
        <div className="action-buttons">
          {reserved ? (
            <span
              className="subtle"
              title="Prepared reserve is managed by the warm pool. Allocate a new workspace to claim a ready instance."
            >
              Reserved for allocation
            </span>
          ) : !instance.managed ? (
            <span
              className="subtle"
              title="This instance is not managed by Engine Box. Lifecycle controls are disabled."
            >
              Read only
            </span>
          ) : busy ? (
            <span className="working">
              <span className="spinner" />
              Working
            </span>
          ) : (
            <>
              <button className="button small" onClick={connect}>
                Manage agents
              </button>
              {resumable && (
                <button
                  className="button small"
                  disabled={!host.online}
                  onClick={() => action(host.id, instance.name, "resume")}
                >
                  Resume
                </button>
              )}
              {running && (
                <>
                  <button
                    className="button small"
                    disabled={!host.online}
                    onClick={() => action(host.id, instance.name, "pause")}
                  >
                    Pause
                  </button>
                  <button
                    className="button small"
                    disabled={!host.online}
                    onClick={() => action(host.id, instance.name, "hibernate")}
                  >
                    Hibernate
                  </button>
                  <button
                    className="button small danger-text"
                    disabled={!host.online}
                    onClick={() => {
                      if (
                        window.confirm(
                          `Stop ${instance.name}? Running processes will end. Use Hibernate to preserve memory.`,
                        )
                      )
                        action(host.id, instance.name, "stop");
                    }}
                  >
                    Stop
                  </button>
                </>
              )}
              {!running && !resumable && (
                <span className="subtle">No actions available</span>
              )}
            </>
          )}
        </div>
      </td>
    </tr>
  );
}

export function hostAdmitting(fleet: Fleet, host: Host) {
  const machine = fleet.compute_nodes?.find(node => node.backend === "incus" && node.source_id === host.id);
  return host.online && (machine?.registration.method !== "enrollment" ||
    (machine.online && ["ready", "capacity_limited"].includes(machine.runtime.state)));
}

export function CreateWorkspace({
  fleet,
  busy,
  close,
  create,
}: {
  fleet: Fleet;
  busy: boolean;
  close: () => void;
  create: (host: string, request: Allocation) => Promise<void>;
}) {
  const [host, setHost] = useState(
    fleet.hosts.find((item) => hostAdmitting(fleet, item))?.id ?? "",
  );
  const available = fleet.templates.filter(
    (item) => !item.host_ids || item.host_ids.includes(host),
  );
  const [template, setTemplate] = useState(available[0]?.id ?? "");
  const selectedTemplate = available.find((item) => item.id === template);
  const requirements =
    selectedTemplate?.host_resources?.[host] ?? selectedTemplate;
  const [size, setSize] = useState(
    requirements?.recommended
      ? "recommended"
      : fleet.sizes.standard
        ? "standard"
        : (Object.keys(fleet.sizes)[0] ?? ""),
  );
  const [custom, setCustom] = useState({
    cpu: "4",
    memory_gib: "4",
    disk_gib: "40",
  });
  const [name, setName] = useState("");
  const selectedHost = fleet.hosts.find((item) => item.id === host);
  useEffect(() => {
    if (!available.some((item) => item.id === template))
      setTemplate(available[0]?.id ?? "");
  }, [host, template, fleet.templates]);
  useEffect(() => {
    const recommended = requirements?.recommended;
    setSize(
      recommended
        ? "recommended"
        : fleet.sizes.standard
          ? "standard"
          : (Object.keys(fleet.sizes)[0] ?? ""),
    );
    if (recommended)
      setCustom({
        cpu: String(recommended.cpu),
        memory_gib: String(recommended.memory_gib),
        disk_gib: String(recommended.disk_gib),
      });
  }, [template, host]);
  const minimum = requirements?.minimum;
  const resources: Resources | undefined =
    size === "custom"
      ? {
          cpu: Number(custom.cpu),
          memory_gib: Number(custom.memory_gib),
          disk_gib: Number(custom.disk_gib),
        }
      : size === "minimum"
        ? minimum
        : size === "recommended"
          ? requirements?.recommended
          : undefined;
  const resourceValid =
    !resources ||
    validResources(
      resources,
      minimum ?? { cpu: 1, memory_gib: 1, disk_gib: 1 },
    );
  const slots = resources
    ? coldSlots(selectedHost, resources)
    : (selectedHost?.capacity?.max_additional[size] ?? 0);
  const pools = matchingPools(selectedHost, template, size, resources);
  const ready = pools.reduce((sum, pool) => sum + pool.ready, 0);
  const preparing = pools.reduce((sum, pool) => sum + pool.preparing, 0);
  const valid =
    !!selectedHost && hostAdmitting(fleet, selectedHost) &&
    !!selectedTemplate &&
    !!size &&
    resourceValid &&
    (ready > 0 || slots > 0);
  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!valid || busy) return;
    await create(
      host,
      size === "custom" && resources
        ? { name, template, resources }
        : { name, template, size },
    );
  }
  return (
    <section className="create-panel" aria-labelledby="create-title">
      <div className="section-title">
        <div>
          <h2 id="create-title">Create a workspace</h2>
          <p>A private workspace from a prepared, credential-free template.</p>
        </div>
        <button
          className="icon-button"
          onClick={close}
          aria-label="Close workspace form"
        >
          <Icon name="close" />
        </button>
      </div>
      <form onSubmit={submit}>
        <div className="create-grid">
          <label>
            Workspace name
            <input
              required
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="ebox-my-project"
              pattern={"ebox-[a-z0-9][a-z0-9\\-]{0,48}"}
              title="Start with ebox-, then use lowercase letters, digits or hyphens."
              maxLength={54}
              autoFocus
            />
          </label>
          <label>
            Host
            <select
              value={host}
              onChange={(event) => setHost(event.target.value)}
              required
            >
              <option value="" disabled>
                Select a host
              </option>
              {fleet.hosts.map((item) => (
                <option key={item.id} value={item.id} disabled={!hostAdmitting(fleet, item)}>
                  {item.label}
                  {!hostAdmitting(fleet, item) ? " (unavailable)" : ""}
                </option>
              ))}
            </select>
          </label>
          <label>
            Prepared template
            <select
              value={template}
              onChange={(event) => setTemplate(event.target.value)}
              required
            >
              <option value="" disabled>
                No installed templates
              </option>
              {available.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.label}
                </option>
              ))}
            </select>
          </label>
          <label>
            Size
            <select
              value={size}
              onChange={(event) => setSize(event.target.value)}
              required
            >
              {minimum && requirements?.recommended ? (
                <>
                  <option value="minimum">
                    Minimum · {minimum.cpu} vCPU / {minimum.memory_gib} GiB /{" "}
                    {minimum.disk_gib} GiB disk
                  </option>
                  <option value="recommended">
                    Recommended · {requirements.recommended.cpu} vCPU /{" "}
                    {requirements.recommended.memory_gib} GiB /{" "}
                    {requirements.recommended.disk_gib} GiB disk
                  </option>
                  <option value="custom">Custom</option>
                </>
              ) : (
                Object.entries(fleet.sizes).map(([id, value]) => (
                  <option key={id} value={id}>
                    {title(id)} · {value.cpu} vCPU / {value.memory_gib} GiB
                  </option>
                ))
              )}
            </select>
          </label>
        </div>
        {size === "custom" && minimum && (
          <div className="custom-resources">
            {(
              [
                ["cpu", "CPU cores"],
                ["memory_gib", "Memory (GiB)"],
                ["disk_gib", "Disk (GiB)"],
              ] as const
            ).map(([key, label]) => (
              <label key={key}>
                {label}
                <input
                  type="number"
                  min={minimum[key]}
                  step="1"
                  required
                  value={custom[key]}
                  onChange={(event) =>
                    setCustom((current) => ({
                      ...current,
                      [key]: event.target.value,
                    }))
                  }
                />
                <span>Minimum {minimum[key]}</span>
              </label>
            ))}
          </div>
        )}
        {resources && resourceValid && (
          <p className="resource-preview">
            {resources.cpu} vCPU · {resources.memory_gib} GiB RAM ·{" "}
            {resources.disk_gib} GiB disk
          </p>
        )}
        <div className="create-footer">
          <div className="allocation-hint" aria-live="polite">
            <p>
              {!available.length
                ? "Install a template on this host from the template catalog first."
                : !selectedHost?.online
                  ? "Choose an online host."
                  : !resourceValid
                    ? "Custom resources must be whole numbers at or above the template minimum."
                    : ready > 0
                      ? `${ready} ready paused workspace${ready === 1 ? " matches" : "s match"} this template and resource allocation. A ready workspace will be claimed if still available.`
                      : slots > 0
                        ? `Cold start: no matching workspace is ready. Capacity is available for ${slots} more workspace${slots === 1 ? "" : "s"} with these resources.`
                        : "No ready workspace or cold-start capacity is available for this selection."}
            </p>
            {ready > 0 && (
              <p>
                {slots > 0
                  ? "If the reserve is claimed first, the request may use a cold start."
                  : "No cold-start capacity remains; another request may claim the reserve first."}
              </p>
            )}
            {preparing > 0 && (
              <p>
                {preparing} matching workspace
                {preparing === 1 ? " is" : "s are"} preparing. Availability
                refreshes automatically.
              </p>
            )}
            {pools
              .filter((pool) => pool.error)
              .map((pool) => (
                <p className="pool-error" key={`${pool.template}/${pool.size}`}>
                  {pool.error}
                </p>
              ))}
          </div>
          <button
            className="button primary"
            disabled={!valid || busy}
            type="submit"
          >
            {busy ? (
              <span className="spinner" />
            ) : (
              <Icon name="plus" size={16} />
            )}
            Create workspace
          </button>
        </div>
      </form>
    </section>
  );
}
