// The server's SQL endpoint, and a hook that runs a query when its text changes (debounced, the
// stale ones dropped), for panels that follow the view.

import { useEffect, useRef, useState, useSyncExternalStore } from "react";

// requests to the server in flight, for the header's spinner
let inFlight = 0;
const listeners = new Set<() => void>();
const notify = () => listeners.forEach((f) => f());

/** Count `p` as in flight until it settles. */
export async function tracked<T>(p: Promise<T>): Promise<T> {
  inFlight++;
  notify();
  try {
    return await p;
  } finally {
    inFlight--;
    notify();
  }
}

/** Whether any request to the server is in flight. */
export function useInFlight(): boolean {
  return useSyncExternalStore(
    (f) => {
      listeners.add(f);
      return () => listeners.delete(f);
    },
    () => inFlight > 0,
  );
}

export interface Result { columns: string[]; rows: unknown[][]; truncated: boolean; ms: number }

/** `source` names who asks, for the server's query log: `page` for the page's own panels. An empty
 * `project` queries the log alone (the `queries` view). */
export async function sql(project: string, run: string | null, query: string, maxRows?: number, signal?: AbortSignal, source = "page"): Promise<Result> {
  const qs = new URLSearchParams();
  if (project) qs.set("project", project);
  if (run) qs.set("run", run);
  if (maxRows) qs.set("max_rows", String(maxRows));
  // in flight until the body is read, not only until the headers arrive
  return tracked(
    (async () => {
      const res = await fetch(`/api/sql?${qs}`, { method: "POST", body: query, signal, headers: { "X-Prow-Source": source } });
      const body = await res.json();
      if (!res.ok) throw new Error(body.error ?? `${res.status}`);
      return body as Result;
    })(),
  );
}

/** Rows as objects by column name. */
export function objects(r: Result): Record<string, unknown>[] {
  return r.rows.map((row) => Object.fromEntries(r.columns.map((c, i) => [c, row[i]])));
}

/** A value from the endpoint as a number (big integers arrive as strings). */
export const n = (v: unknown): number => (typeof v === "number" ? v : typeof v === "string" ? Number(v) : NaN);

/** The SQL for an absolute time in µs since the epoch, as a TIMESTAMPTZ constant (so DuckDB can
 * skip row groups by `ts`). */
export const at = (us: number) => `to_timestamp(${(us / 1e6).toFixed(6)})`;

/** A string as a SQL literal. */
export const lit = (s: string) => `'${s.replace(/'/g, "''")}'`;

/** A quoted SQL name. */
export const ident = (s: string) => `"${s.replace(/"/g, '""')}"`;

/**
 * Run `make()`'s query whenever `key` changes, `delay` ms after the last change; a newer key
 * aborts an older query. `data` keeps the last result while the next is on its way.
 */
export function useQuery<T>(key: string | null, make: (signal: AbortSignal) => Promise<T>, delay = 120): { data: T | null; error: string | null; loading: boolean } {
  const [state, setState] = useState<{ data: T | null; error: string | null; loading: boolean }>({ data: null, error: null, loading: key !== null });
  const makeRef = useRef(make);
  makeRef.current = make;
  useEffect(() => {
    if (key === null) return;
    const ctl = new AbortController();
    setState((s) => ({ ...s, loading: true }));
    const timer = setTimeout(() => {
      makeRef.current(ctl.signal).then(
        (data) => !ctl.signal.aborted && setState({ data, error: null, loading: false }),
        (e) => !ctl.signal.aborted && setState((s) => ({ ...s, error: String(e), loading: false })),
      );
    }, delay);
    return () => {
      clearTimeout(timer);
      ctl.abort();
    };
  }, [key, delay]);
  return state;
}
