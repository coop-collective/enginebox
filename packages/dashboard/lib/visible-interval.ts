import { useEffect, useRef, useState } from "react";

/** True while the tab is visible; use to hold back self-rescheduling pollers. */
export function usePageVisible(): boolean {
  const [visible, setVisible] = useState(true);
  useEffect(() => {
    const sync = () => setVisible(!document.hidden);
    sync();
    document.addEventListener("visibilitychange", sync);
    return () => document.removeEventListener("visibilitychange", sync);
  }, []);
  return visible;
}

/**
 * Calls `callback` every `delayMs` while the page is visible. The timer stops
 * while `document.hidden` and the callback runs immediately when the tab
 * becomes visible again. Pass `null` to disable.
 */
export function useVisibleInterval(
  callback: () => void,
  delayMs: number | null,
) {
  const latest = useRef(callback);
  latest.current = callback;
  useEffect(() => {
    if (delayMs === null) return;
    let timer: ReturnType<typeof setInterval> | undefined;
    const stop = () => {
      clearInterval(timer);
      timer = undefined;
    };
    const start = () => {
      stop();
      timer = setInterval(() => latest.current(), delayMs);
    };
    const onVisibility = () => {
      if (document.hidden) return stop();
      latest.current();
      start();
    };
    if (!document.hidden) start();
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [delayMs]);
}
