"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { Icon } from "@/components/ui";

export type Command = { id: string; label: string; group: string; icon: string; keywords?: string };

/** Jump to any view by name. Opens with ⌘K / Ctrl+K or "/", closes with Escape. */
export function CommandMenu({ open, commands, close, run }: { open: boolean; commands: Command[]; close: () => void; run: (id: string) => void }) {
  const [query, setQuery] = useState("");
  const [index, setIndex] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const results = useMemo(() => {
    const q = query.trim().toLowerCase();
    return q ? commands.filter((c) => `${c.label} ${c.group} ${c.keywords ?? ""}`.toLowerCase().includes(q)) : commands;
  }, [commands, query]);
  useEffect(() => {
    if (!open) return;
    setQuery("");
    setIndex(0);
    input.current?.focus();
  }, [open]);
  useEffect(() => setIndex(0), [query]);
  if (!open) return null;
  function choose(id: string | undefined) {
    if (!id) return;
    close();
    run(id);
  }
  return (
    <div className="command-scrim" onMouseDown={close}>
      <div className="command-menu" role="dialog" aria-modal="true" aria-label="Jump to" onMouseDown={(e) => e.stopPropagation()}>
        <div className="command-search">
          <Icon name="search" size={16} />
          <input
            ref={input}
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Jump to a page…"
            aria-label="Jump to a page"
            role="combobox"
            aria-expanded="true"
            aria-controls="command-results"
            aria-activedescendant={results[index] ? `command-${results[index].id}` : undefined}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setIndex((i) => Math.min(results.length - 1, i + 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setIndex((i) => Math.max(0, i - 1));
              } else if (e.key === "Enter") {
                e.preventDefault();
                choose(results[index]?.id);
              } else if (e.key === "Escape") {
                e.preventDefault();
                close();
              }
            }}
          />
          <kbd>Esc</kbd>
        </div>
        <ul id="command-results" role="listbox" aria-label="Pages">
          {results.map((command, i) => (
            <li
              key={command.id}
              id={`command-${command.id}`}
              role="option"
              aria-selected={i === index}
              onMouseEnter={() => setIndex(i)}
              onClick={() => choose(command.id)}
            >
              <Icon name={command.icon} size={16} />
              <span>{command.label}</span>
              <small>{command.group}</small>
            </li>
          ))}
          {!results.length && <li className="command-empty">No page matches “{query}”.</li>}
        </ul>
      </div>
    </div>
  );
}
