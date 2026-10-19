"use client";

import { FormEvent, useCallback, useEffect, useState } from "react";
import { api } from "@/lib/client";
import type { Host, Resources, TemplateRecord } from "@/lib/types";

function resources(value: Resources) {
  return `${value.cpu} vCPU · ${value.memory_gib} GiB RAM · ${value.disk_gib} GiB disk`;
}

function TemplateEntry({
  record,
  hosts,
  installing,
  install,
}: {
  record: TemplateRecord;
  hosts: Host[];
  installing: string[];
  install: (host: string, template: string) => Promise<boolean>;
}) {
  const [host, setHost] = useState(hosts.find((item) => item.online)?.id ?? "");
  const [submitting, setSubmitting] = useState(false);
  const selected = hosts.find((item) => item.id === host);
  const deployment = record.deployments?.find((item) => item.host_id === host);
  const busy = submitting || installing.includes(`${host}/${record.id}`);
  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!selected?.online || busy || deployment?.in_sync) return;
    setSubmitting(true);
    try {
      await install(host, record.id);
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
      <form className="template-install" onSubmit={submit}>
        <label>
          Install host
          <select
            value={host}
            onChange={(event) => setHost(event.target.value)}
            required
          >
            <option value="" disabled>
              Select a host
            </option>
            {hosts.map((item) => (
              <option key={item.id} value={item.id} disabled={!item.online}>
                {item.label}
                {!item.online ? " (offline)" : ""}
              </option>
            ))}
          </select>
        </label>
        <button
          className="button"
          type="submit"
          disabled={!selected?.online || busy || !!deployment?.in_sync}
        >
          {busy
            ? "Installing…"
            : deployment?.in_sync
              ? "Installed"
              : deployment
                ? "Sync template"
                : "Install template"}
        </button>
      </form>
      <p
        className={`deployment-status ${deployment && !deployment.in_sync ? "out-of-sync" : ""}`}
      >
        {deployment
          ? `Installed revision ${deployment.revision} · ${deployment.in_sync ? "Up to date" : "Out of sync"}`
          : "Not installed on this host."}
      </p>
      {deployment && (
        <p className="mono deployment-source">
          {deployment.source}/{deployment.snapshot}
        </p>
      )}
    </article>
  );
}

export function TemplateCatalog({
  hosts,
  refreshToken,
  installing,
  install,
  registered,
  close,
}: {
  hosts: Host[];
  refreshToken: number;
  installing: string[];
  install: (host: string, template: string) => Promise<boolean>;
  registered: () => void;
  close: () => void;
}) {
  const [records, setRecords] = useState<TemplateRecord[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [source, setSource] = useState("url");
  const [url, setUrl] = useState("");
  const [manifest, setManifest] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const load = useCallback(async () => {
    setLoading(true);
    try {
      const data = await api<{ templates: TemplateRecord[] }>(
        "/api/control/v1/templates",
      );
      setRecords(data.templates);
      setError("");
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setLoading(false);
    }
  }, []);
  useEffect(() => {
    void load();
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
      setNotice("Template registered. Choose a host to install it.");
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
          <p>Import a template, then install or sync it on a host.</p>
        </div>
        <button className="button small" type="button" onClick={close}>
          Close catalog
        </button>
      </div>
      {error && (
        <div className="alert" role="alert">
          {error}
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
        Registration adds a manifest to the catalog. Installation prepares a
        credential-free base on the selected host; native clients use their own
        authentication.
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
            <TemplateEntry
              key={record.id}
              record={record}
              hosts={hosts}
              installing={installing}
              install={install}
            />
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
