// The server's index, and a run's tables read straight from its Parquet files with hyparquet.
// Times become microseconds from the run's first row: absolute nanoseconds do not fit a JS number.

import { parquetMetadata, parquetReadObjects } from "hyparquet";

export type Manifest = Record<string, string | number | boolean | null>;
export interface RunEntry { manifest: Manifest; sites: Record<string, string[]> }
export interface Project { name: string; runs: RunEntry[] }

export async function loadIndex(): Promise<Project[]> {
  const res = await fetch("/api/index");
  if (!res.ok) throw new Error(`index: ${res.status}`);
  return (await res.json()).projects;
}

export type Cell = number | string | boolean;
/** A site's table: its column names and types, and its rows as columns. */
export interface Table {
  site: string;
  fields: { name: string; kind: "number" | "label" | "bool" | "time" }[];
  /** µs from the run's start, one a row. */
  t: Float64Array;
  cols: Record<string, Cell[]>;
}

export interface Zone { lane: string; name: string; category: string; t: number; dur: number; depth: number; parent: number }
export interface Run {
  entry: RunEntry;
  /** ns since the epoch of t = 0, as a bigint. */
  t0: bigint;
  /** µs: the run's last row (a zone's end included). */
  end: number;
  zones: Zone[];
  lanes: string[];
  plots: Table | null;
  messages: Table | null;
  sites: Table[];
}

const BUILTIN = new Set(["zones", "plots", "messages"]);

type Raw = Record<string, unknown>[];
/** A site's rows, and which of its columns are timestamps (from the Parquet schema). */
interface Read { rows: Raw; times: Set<string> }

async function readSite(files: string[]): Promise<Read> {
  const rows: Raw = [];
  const times = new Set<string>();
  for (const f of files) {
    const buf = await (await fetch(`/data/${f}`)).arrayBuffer();
    for (const el of parquetMetadata(buf).schema) if (el.logical_type?.type === "TIMESTAMP") times.add(el.name);
    for (const r of await parquetReadObjects({ file: buf, parsers: { timestampFromNanoseconds: (n: bigint) => n } })) rows.push(r);
  }
  return { rows, times };
}

function toTable(site: string, { rows, times }: Read, t0: bigint): Table {
  const names = rows.length ? Object.keys(rows[0]).filter((k) => k !== "ts") : [];
  const fields = names.map((name) => {
    const v = rows[0][name];
    const kind = times.has(name) ? "time" : typeof v === "string" ? "label" : typeof v === "boolean" ? "bool" : "number";
    return { name, kind } as Table["fields"][number];
  });
  const t = new Float64Array(rows.length);
  const cols: Record<string, Cell[]> = Object.fromEntries(names.map((n) => [n, new Array(rows.length)]));
  rows.forEach((r, i) => {
    t[i] = Number((r.ts as bigint) - t0) / 1e3;
    for (const n of names) {
      const v = r[n];
      cols[n][i] = times.has(n) ? Number((v as bigint) - t0) / 1e3 : typeof v === "bigint" ? Number(v) : (v as Cell);
    }
  });
  return { site, fields, t, cols };
}

export async function loadRun(entry: RunEntry): Promise<Run> {
  const raw: Record<string, Read> = {};
  await Promise.all(Object.entries(entry.sites).map(async ([s, files]) => (raw[s] = await readSite(files))));
  let t0: bigint | null = null;
  for (const { rows } of Object.values(raw)) for (const r of rows) if (t0 === null || (r.ts as bigint) < t0) t0 = r.ts as bigint;
  t0 ??= 0n;
  const tables = Object.fromEntries(Object.entries(raw).map(([s, rows]) => [s, toTable(s, rows, t0!)]));

  const zones: Zone[] = [];
  const z = tables["zones"];
  if (z) {
    for (let i = 0; i < z.t.length; i++)
      zones.push({ lane: z.cols.lane[i] as string, name: z.cols.name[i] as string, category: z.cols.category[i] as string, t: z.t[i], dur: (z.cols.dur_ns[i] as number) / 1e3, depth: 0, parent: -1 });
  }
  nest(zones);
  const lanes = [...new Set(zones.map((z) => z.lane))].sort((a, b) => laneOrder(a) - laneOrder(b) || a.localeCompare(b));

  let end = 0;
  for (const tb of Object.values(tables)) for (const t of tb.t) end = Math.max(end, t);
  for (const zn of zones) end = Math.max(end, zn.t + zn.dur);

  return {
    entry, t0, end: Math.max(end, 1), zones, lanes,
    plots: tables["plots"] ?? null,
    messages: tables["messages"] ?? null,
    sites: Object.values(tables).filter((t) => !BUILTIN.has(t.site)),
  };
}

/** Host threads first (`main` at the top), then the device lanes. */
function laneOrder(l: string): number {
  return l === "main" ? 0 : /^(card|gpu)/.test(l) ? 2 : 1;
}

/** Depth and parent of each zone on its lane, by containment: the flame chart's rows. */
function nest(zones: Zone[]) {
  const order = zones.map((_, i) => i).sort((a, b) => zones[a].lane.localeCompare(zones[b].lane) || zones[a].t - zones[b].t || zones[b].dur - zones[a].dur);
  let lane = "";
  const stack: number[] = [];
  for (const i of order) {
    const zn = zones[i];
    if (zn.lane !== lane) { lane = zn.lane; stack.length = 0; }
    while (stack.length && zones[stack[stack.length - 1]].t + zones[stack[stack.length - 1]].dur <= zn.t) stack.pop();
    zn.depth = stack.length;
    zn.parent = stack.length ? stack[stack.length - 1] : -1;
    stack.push(i);
  }
}
