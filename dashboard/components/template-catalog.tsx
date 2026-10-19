"use client";

import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import { api } from "@/lib/client";
import type { Resources, TemplateRecord } from "@/lib/types";

function resources(value: Resources) {
  return `${value.cpu} vCPU · ${value.memory_gib} GiB RAM · ${value.disk_gib} GiB disk`;
}

function downloadTemplate(record: TemplateRecord) {
  // Catalog records also contain private deployment receipts. Export only the
  // portable v1 manifest so the file can be registered on another installation.
  const keys = [
    "schema_version",
    "id",
    "revision",
    "label",
    "description",
    "recipe",
    "source_image",
    "minimum",
    "recommended",
    "native_clients",
    "config",
  ] as const;
  const manifest = Object.fromEntries(keys.map((key) => [key, record[key]]));
  const url = URL.createObjectURL(
    new Blob([JSON.stringify(manifest, null, 2) + "\n"], {
      type: "application/json",
    }),
  );
  const link = document.createElement("a");
  link.href = url;
  link.download = `${record.id}-r${record.revision}.json`;
  document.body.appendChild(link);
  try {
    link.click();
  } finally {
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 0);
  }
}

function TemplateEntry({
  record,
  refresh,
}: {
  record: TemplateRecord;
  refresh: () => Promise<void>;
}) {
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState("");
  const nodes = record.sync?.nodes ?? [];
  const ready = nodes.filter((node) => node.status === "ready").length;
  const busy = submitting || nodes.some((node) => node.status === "syncing");
  async function retry() {
    setSubmitting(true);
    setError("");
    try {
      await api(
        `/api/control/v1/templates/${encodeURIComponent(record.id)}/sync`,
        { method: "POST", body: "{}" },
      );
      await refresh();
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setSubmitting(false);
    }
  }
  return (
    <article className="catalog-card">
      <div className="catalog-card-heading">
        <div>
          <h3>{record.label}</h3>
          <span className="mono">
            {record.id} · revision {record.revision}
          </span>
        </div>
        <span className="catalog-recipe">{record.recipe}</span>
        <button
          className="button small"
          type="button"
          onClick={() => downloadTemplate(record)}
        >
          Export JSON
        </button>
      </div>
      {record.description && (
        <p className="catalog-description">{record.description}</p>
      )}
      <dl className="template-resources">
        <div>
          <dt>Minimum</dt>
          <dd>{resources(record.minimum)}</dd>
        </div>
        <div>
          <dt>Recommended</dt>
          <dd>{resources(record.recommended)}</dd>
        </div>
      </dl>
      {!!record.native_clients?.length && (
        <p className="catalog-description">
          Native clients: {record.native_clients.join(", ")}
        </p>
      )}
      <div className="template-sync-heading">
        <p className="deployment-status">
          {record.sync
            ? `${ready} / ${nodes.length} nodes ready · target revision ${record.sync.desired_revision}`
            : "Synchronization status has not been reported."}
        </p>
        <button
          className="button small"
          type="button"
          disabled={busy || (nodes.length > 0 && ready === nodes.length)}
          onClick={retry}
        >
          {submitting ? "Requesting sync…" : "Retry sync"}
        </button>
      </div>
      {error && (
        <p className="account-error" role="alert">
          {error}
        </p>
      )}
      {nodes.length ? (
        <ul
          className="template-nodes"
          aria-label={`${record.label} node synchronization`}
        >
          {nodes.map((node) => {
            const deployment = record.deployments?.find(
              (item) => item.host_id === node.host_id,
            );
            return (
              <li
                key={node.host_id}
                aria-label={`${node.label} synchronization`}
              >
                <div className="template-node-heading">
                  <strong>{node.label}</strong>
                  <span className={`node-status node-${node.status}`}>
                    {node.status.charAt(0).toUpperCase() + node.status.slice(1)}
                  </span>
                </div>
                {deployment && (
                  <p className="deployment-status">
                    Installed revision {deployment.revision} ·{" "}
                    {node.status === "ready" && deployment.in_sync
                      ? "Up to date"
                      : "Awaiting synchronization"}
                  </p>
                )}
                {node.error && <p className="account-error">{node.error}</p>}
              </li>
            );
          })}
        </ul>
      ) : (
        record.sync && (
          <p className="catalog-description">
            No nodes registered. This template will sync when a node joins.
          </p>
        )
      )}
    </article>
  );
}

export function TemplateCatalog({
  refreshToken,
  registered,
  close,
}: {
  refreshToken: number;
  registered: () => void;
  close: () => void;
}) {
  const [records, setRecords] = useState<TemplateRecord[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [loadError, setLoadError] = useState("");
  const [notice, setNotice] = useState("");
  const [source, setSource] = useState("url");
  const [url, setUrl] = useState("");
  const [manifest, setManifest] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const current = useRef<AbortController | null>(null);
  const load = useCallback(async () => {
    if (current.current) return;
    const controller = new AbortController();
    current.current = controller;
    setLoading(true);
    try {
      const data = await api<{ templates: TemplateRecord[] }>(
        "/api/control/v1/templates",
        { signal: controller.signal },
      );
      if (!controller.signal.aborted) {
        setRecords(data.templates);
        setLoadError("");
      }
    } catch (err) {
      if (!controller.signal.aborted) {
        setLoadError((err as Error).message);
        setRecords(null);
      }
    } finally {
      if (current.current === controller) current.current = null;
      if (!controller.signal.aborted) setLoading(false);
    }
  }, []);
  useEffect(() => {
    void load();
    const timer = setInterval(() => void load(), 2000);
    return () => {
      clearInterval(timer);
      current.current?.abort();
      current.current = null;
    };
  }, [load, refreshToken]);
  async function register(event: FormEvent) {
    event.preventDefault();
    setSubmitting(true);
    setError("");
    setNotice("");
    try {
      let body;
      if (source === "url") body = { url: url.trim() };
      else {
        let parsed;
        try {
          parsed = JSON.parse(manifest);
        } catch {
          throw new Error("The template must be valid JSON.");
        }
        if (!parsed || typeof parsed !== "object" || Array.isArray(parsed))
          throw new Error("The template must be a JSON object.");
        body = { manifest: parsed };
      }
      await api("/api/control/v1/templates", {
        method: "POST",
        body: JSON.stringify(body),
      });
      await load();
      setNotice(
        "Template registered. Automatic synchronization is queued across all nodes.",
      );
      registered();
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setSubmitting(false);
    }
  }
  return (
    <section className="catalog-panel" aria-labelledby="catalog-title">
      <div className="section-title">
        <div>
          <h2 id="catalog-title">Template catalog</h2>
          <p>
            Register once. Templates automatically sync to every registered
            node, including nodes added later.
          </p>
        </div>
        <button className="button small" type="button" onClick={close}>
          Close catalog
        </button>
      </div>
      {error && (
        <div className="alert" role="alert">
          {error}
        </div>
      )}
      {loadError && (
        <div className="alert" role="alert">
          {loadError}
          <button className="text-button" onClick={load}>
            Retry catalog
          </button>
        </div>
      )}
      {notice && (
        <p className="catalog-notice" role="status">
          {notice}
        </p>
      )}
      <form className="template-import" onSubmit={register}>
        <label>
          Import from
          <select
            value={source}
            onChange={(event) => setSource(event.target.value)}
          >
            <option value="url">HTTPS URL</option>
            <option value="json">JSON manifest</option>
          </select>
        </label>
        {source === "url" ? (
          <label className="import-content">
            Template URL
            <input
              type="url"
              pattern="https://.*"
              placeholder="https://example.com/template.json"
              value={url}
              onChange={(event) => setUrl(event.target.value)}
              required
            />
          </label>
        ) : (
          <label className="import-content">
            Template JSON
            <textarea
              value={manifest}
              onChange={(event) => setManifest(event.target.value)}
              placeholder={'{"schema_version":1,"id":"native-dev",...}'}
              rows={6}
              maxLength={1024 * 1024}
              required
              spellCheck={false}
            />
          </label>
        )}
        <button className="button primary" type="submit" disabled={submitting}>
          {submitting ? "Registering…" : "Register template"}
        </button>
      </form>
      <p className="workspace-hint">
        The controller keeps the canonical template. Each node prepares a
        credential-free base; workspace creation becomes available only after
        that node reports ready.
      </p>
      {records === null ? (
        <p className="catalog-description">
          {loading
            ? "Loading template catalog…"
            : "Template catalog is unavailable."}
        </p>
      ) : records.length ? (
        <div className="catalog-grid">
          {records.map((record) => (
            <TemplateEntry key={record.id} record={record} refresh={load} />
          ))}
        </div>
      ) : (
        <div className="empty-state compact">
          <h3>No registered templates</h3>
          <p>
            Import a serialized template using its HTTPS URL or JSON manifest.
          </p>
        </div>
      )}
    </section>
  );
}
