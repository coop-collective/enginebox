"use client";

import type { ReactNode } from "react";

/** Line icons on a 24px grid, 1.6px stroke. Each name is used for one meaning. */
const paths: Record<string, ReactNode> = {
  box: (
    <>
      <path d="m12 3 9 5v8l-9 5-9-5V8l9-5Z" />
      <path d="m3 8 9 5 9-5M12 13v8" />
    </>
  ),
  overview: (
    <>
      <rect x="3" y="3" width="8" height="8" rx="1.5" />
      <rect x="13" y="3" width="8" height="5" rx="1.5" />
      <rect x="13" y="10" width="8" height="11" rx="1.5" />
      <rect x="3" y="13" width="8" height="8" rx="1.5" />
    </>
  ),
  server: (
    <>
      <rect x="3" y="3" width="18" height="7" rx="2" />
      <rect x="3" y="14" width="18" height="7" rx="2" />
      <path d="M7 6.5h.01M7 17.5h.01M12 6.5h5M12 17.5h5" />
    </>
  ),
  vm: (
    <>
      <rect x="3" y="4" width="18" height="13" rx="2" />
      <path d="M8 21h8M12 17v4M7 9h4M7 12h7" />
    </>
  ),
  template: (
    <>
      <path d="M4 7h16M4 12h16M4 17h10" />
      <circle cx="18" cy="17" r="2" />
    </>
  ),
  folder: <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z" />,
  globe: (
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M3 12h18M12 3c2.5 3 2.5 15 0 18M12 3c-2.5 3-2.5 15 0 18" />
    </>
  ),
  agent: (
    <>
      <rect x="5" y="8" width="14" height="11" rx="3" />
      <path d="M12 4v4M9 13h.01M15 13h.01M9.5 16h5" />
    </>
  ),
  profile: (
    <>
      <path d="M4 6h10M4 12h7M4 18h10" />
      <circle cx="17" cy="6" r="2" />
      <circle cx="14" cy="12" r="2" />
      <circle cx="17" cy="18" r="2" />
    </>
  ),
  branch: (
    <>
      <circle cx="6" cy="5" r="2" />
      <circle cx="6" cy="19" r="2" />
      <circle cx="18" cy="8" r="2" />
      <path d="M6 7v10M18 10c0 4-6 3-11 7" />
    </>
  ),
  check: <path d="m5 12 4 4L19 6" />,
  checks: (
    <>
      <path d="m3 12 3 3 6-7M11 15l1 1 9-10" />
    </>
  ),
  key: (
    <>
      <circle cx="8" cy="15" r="4" />
      <path d="m11 12 9-9M16 7l3 3M14 9l2 2" />
    </>
  ),
  plug: <path d="M9 3v5M15 3v5M6 8h12v3a6 6 0 0 1-12 0V8ZM12 17v4" />,
  team: (
    <>
      <circle cx="9" cy="8" r="3" />
      <path d="M3 20a6 6 0 0 1 12 0M16 4.5a3 3 0 0 1 0 6M18 14a5 5 0 0 1 3 6" />
    </>
  ),
  link: (
    <>
      <path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1" />
      <path d="M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1" />
    </>
  ),
  plus: <path d="M12 5v14M5 12h14" />,
  refresh: (
    <>
      <path d="M20 7v5h-5M4 17v-5h5" />
      <path d="M6 7a7 7 0 0 1 12-1l2 3M4 15l2 3a7 7 0 0 0 12-1" />
    </>
  ),
  cpu: (
    <>
      <rect x="6" y="6" width="12" height="12" rx="2" />
      <path d="M10 2v4m4-4v4m-4 12v4m4-4v4M2 10h4m-4 4h4m12-4h4m-4 4h4" />
    </>
  ),
  memory: (
    <>
      <rect x="3" y="6" width="18" height="11" rx="2" />
      <path d="M7 10v3m5-3v3m5-3v3M7 17v3m5-3v3m5-3v3" />
    </>
  ),
  lock: (
    <>
      <rect x="5" y="10" width="14" height="11" rx="2" />
      <path d="M8 10V7a4 4 0 0 1 8 0v3" />
    </>
  ),
  mail: (
    <>
      <rect x="3" y="5" width="18" height="14" rx="2" />
      <path d="m4 7 8 6 8-6" />
    </>
  ),
  search: (
    <>
      <circle cx="11" cy="11" r="6" />
      <path d="m20 20-4.5-4.5" />
    </>
  ),
  close: <path d="m6 6 12 12M6 18 18 6" />,
  menu: <path d="M4 7h16M4 12h16M4 17h16" />,
  terminal: (
    <>
      <rect x="3" y="4" width="18" height="16" rx="3" />
      <path d="m7 9 3 3-3 3m6 0h4" />
    </>
  ),
  logout: <path d="M10 4H5v16h5m4-12 4 4-4 4m-8-4h12" />,
  sparkle: <path d="M12 3v6M12 15v6M3 12h6M15 12h6" />,
};

export function Icon({ name = "box", size = 18 }: { name?: string; size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      className="icon"
    >
      {paths[name] ?? paths.box}
    </svg>
  );
}

/**
 * Status as an indicator shape plus text, like an LED beside a label.
 * Tone carries one meaning each: ok (done/healthy), agent (working), decal
 * (waiting on a person), danger (failed), neutral (idle/unknown).
 */
export type Tone = "ok" | "agent" | "decal" | "danger" | "neutral" | "info";
export function Tag({ tone = "neutral", live, children }: { tone?: Tone; live?: boolean; children: ReactNode }) {
  return (
    <span className="tag" data-tone={tone} data-live={live ? "" : undefined}>
      <i className="tag-dot" aria-hidden="true" />
      {children}
    </span>
  );
}

export function Meter({ used, total, label }: { used: number; total: number; label?: string }) {
  const percentage = Math.min(100, Math.max(0, total ? (used / total) * 100 : 0));
  return (
    <div className="meter" role="img" aria-label={label ?? `${Math.round(percentage)} percent allocated`}>
      <span style={{ width: `${percentage}%` }} data-high={percentage >= 85 ? "" : undefined} />
    </div>
  );
}

export function PageHeader({ title, description, actions }: { title: string; description?: ReactNode; actions?: ReactNode }) {
  return (
    <div className="page-heading">
      <div>
        <h1>{title}</h1>
        {description && <p>{description}</p>}
      </div>
      {actions && <div className="heading-actions">{actions}</div>}
    </div>
  );
}

/**
 * A row of tabs for one page. Each tab is a real tab (role="tab"), keyed by
 * a URL-safe id so the caller can mirror the selection in the address bar.
 */
export function Tabs({ label, tabs, value, onChange }: { label: string; tabs: { id: string; label: string; count?: number }[]; value: string; onChange: (id: string) => void }) {
  return (
    <div className="tabs" role="tablist" aria-label={label}>
      {tabs.map((tab) => (
        <button
          key={tab.id}
          type="button"
          role="tab"
          id={`tab-${tab.id}`}
          aria-selected={value === tab.id}
          className={`tab${value === tab.id ? " active" : ""}`}
          onClick={() => onChange(tab.id)}
        >
          {tab.label}
          {tab.count != null && <span className="count">{tab.count}</span>}
        </button>
      ))}
    </div>
  );
}
