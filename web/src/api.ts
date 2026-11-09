// The run, as the server's SQL answers it: a summary up front (sites, fields, counts, extents,
// zone lanes), and the rest fetched by the panels as they need it. Times in the page are µs from the
// run's first row; the server's are absolute (`epoch_us(ts)`), and `t0` converts.

import { at, ident, n, objects, sql, tracked } from "./sql";
import { durationScale, isDuration } from "./fmt";

export type Manifest = Record<string, string | number | boolean | null>;
/** `last`: when the run last wrote a file (ISO); `bytes`: its size on disk. */
export interface RunEntry { manifest: Manifest; sites: Record<string, string[]>; last: string | null; bytes: number }
export interface Project { name: string; runs: RunEntry[] }

export async function loadIndex(): Promise<Project[]> {
  return tracked(
    (async () => {
      const res = await fetch("/api/index");
      if (!res.ok) throw new Error(`index: ${res.status}`);
      return (await res.json()).projects as Project[];
    })(),
  );
}

export interface Field { name: string; kind: "number" | "label" | "bool" | "time" }
/** A site of rows: its fields, its row count, and its extent (µs from t0). */
export interface Site {
  site: string;
  fields: Field[];
  /** Its duration fields, and µs in one unit of each. */
  durations: string[];
  scale: number[];
  n: number;
  first: number;
  last: number;
  /** The longest row's span (all its durations together), µs: how far back a row can reach. */
  span: number;
}
export interface Zone { lane: string; name: string; category: string; t: number; dur: number; depth: number; parent: number }

export interface Run {
  entry: RunEntry;
  project: string;
  id: string;
  /** µs since the epoch of the run's first row: t = 0 in the page. */
  t0: number;
  /** µs: the run's last moment (a zone's end included). */
  end: number;
  /** The zones' count, first start, longest duration (µs) and lanes; null without zones. */
  zones: { n: number; first: number; longest: number; lanes: string[] } | null;
  sites: Site[];
  plots: string[];
  /** Message times (µs), for the marks on the charts. */
  marks: number[];
}

const BUILTIN = new Set(["zones", "plots", "messages"]);

function kind(type: string): Field["kind"] {
  if (/TIMESTAMP/i.test(type)) return "time";
  if (/VARCHAR|TEXT|STRING/i.test(type)) return "label";
  if (/BOOL/i.test(type)) return "bool";
  return "number";
}

/** A site's row span in SQL: the sum of its duration fields, in µs. */
export function spanSql(s: Pick<Site, "durations" | "scale">): string {
  return s.durations.length ? s.durations.map((d, i) => `coalesce(${ident(d)}, 0) * ${s.scale[i]}`).join(" + ") : "0";
}

export async function loadRun(project: string, entry: RunEntry): Promise<Run> {
  const id = String(entry.manifest.run);
  const names = Object.keys(entry.sites);
  const cols = objects(await sql(project, id, `SELECT table_name AS t, column_name AS c, data_type AS ty FROM duckdb_columns() WHERE database_name = 'temp' AND table_name <> 'runs' ORDER BY table_name, column_index`));
  const fieldsOf = new Map<string, Field[]>();
  for (const r of cols) {
    const t = String(r.t), c = String(r.c);
    if (c === "ts" || c === "run" || c === "project") continue;
    if (!fieldsOf.has(t)) fieldsOf.set(t, []);
    fieldsOf.get(t)!.push({ name: c, kind: kind(String(r.ty)) });
  }
  const sites: Site[] = names
    .filter((s) => !BUILTIN.has(s) && fieldsOf.has(s))
    .map((s) => {
      const fields = fieldsOf.get(s)!;
      const durations = fields.filter((f) => f.kind === "number" && isDuration(f.name)).map((f) => f.name);
      return { site: s, fields, durations, scale: durations.map(durationScale), n: 0, first: 0, last: 0, span: 0 };
    });
  // every source's count and extent in one query, absolute µs
  const parts: string[] = sites.map((s) => `SELECT ${`'${s.site}'`} AS s, count(*) AS n, min(epoch_us(ts)) AS a, max(epoch_us(ts)) AS b, max(${spanSql(s)}) AS span FROM ${ident(s.site)}`);
  if (names.includes("zones")) parts.push(`SELECT 'zones', count(*), min(epoch_us(ts)), max(epoch_us(ts) + dur_ns / 1000), max(dur_ns / 1000) FROM zones`);
  if (names.includes("plots")) parts.push(`SELECT 'plots', count(*), min(epoch_us(ts)), max(epoch_us(ts)), 0 FROM plots`);
  if (names.includes("messages")) parts.push(`SELECT 'messages', count(*), min(epoch_us(ts)), max(epoch_us(ts)), 0 FROM messages`);
  const ext = new Map(objects(await sql(project, id, parts.join(" UNION ALL "))).map((r) => [String(r.s), r]));
  let t0 = Infinity, last = -Infinity;
  for (const r of ext.values()) {
    if (n(r.n) === 0) continue;
    t0 = Math.min(t0, n(r.a) - (String(r.s) === "zones" ? 0 : n(r.span)));
    last = Math.max(last, n(r.b));
  }
  if (!Number.isFinite(t0)) t0 = last = 0;
  for (const s of sites) {
    const r = ext.get(s.site);
    s.n = n(r?.n);
    s.first = n(r?.a) - t0 - n(r?.span);
    s.last = n(r?.b) - t0;
    s.span = n(r?.span) || 0;
  }
  let zones: Run["zones"] = null;
  const z = ext.get("zones");
  if (z && n(z.n) > 0) {
    const lanes = objects(await sql(project, id, `SELECT DISTINCT lane FROM zones`)).map((r) => String(r.lane));
    lanes.sort((a, b) => laneOrder(a) - laneOrder(b) || a.localeCompare(b));
    zones = { n: n(z.n), first: n(z.a) - t0, longest: n(z.span), lanes };
  }
  const plots = names.includes("plots") ? objects(await sql(project, id, `SELECT DISTINCT name FROM plots ORDER BY name`)).map((r) => String(r.name)) : [];
  const marks = names.includes("messages") ? (await sql(project, id, `SELECT epoch_us(ts) - ${t0} FROM messages ORDER BY ts`, 10_000)).rows.map((r) => n(r[0])) : [];
  return { entry, project, id, t0, end: Math.max(1, last - t0), zones, sites, plots, marks };
}

/** Host threads first (`main` at the top), then the device lanes. */
function laneOrder(l: string): number {
  return l === "main" ? 0 : /^(card|gpu)/.test(l) ? 2 : 1;
}

// ---- tiles: a fixed grid of time slices at power-of-two widths, fetched once each ----------------

/** The zones of one tile: those overlapping [start, start + width) (µs from t0), at least `cutoff` long. */
export async function zoneTile(run: Run, start: number, width: number, cutoff: number, signal?: AbortSignal): Promise<{ items: Zone[]; truncated: boolean }> {
  const a = run.t0 + start, b = a + width, back = run.zones?.longest ?? 0;
  const q = `SELECT lane, name, category, epoch_us(ts) - ${run.t0} AS t, dur_ns / 1000.0 AS dur FROM zones
    WHERE ts >= ${at(a - back)} AND ts < ${at(b)} AND epoch_us(ts) + dur_ns / 1000 > ${a} AND dur_ns >= ${Math.floor(cutoff * 1000)}`;
  const r = await sql(run.project, run.id, q, 100_000, signal);
  return { items: r.rows.map((x) => ({ lane: String(x[0]), name: String(x[1]), category: String(x[2]), t: n(x[3]), dur: n(x[4]), depth: 0, parent: -1 })), truncated: r.truncated };
}

/** A row of a site: its time (µs from t0) and its fields. */
export interface Row { t: number; v: Record<string, number | string | boolean> }

/** A site's rows of one tile: rows whose span (ending at their time) overlaps it, spans at least `cutoff`. */
export async function siteTile(run: Run, s: Site, start: number, width: number, cutoff: number, signal?: AbortSignal): Promise<{ items: Row[]; truncated: boolean }> {
  const a = run.t0 + start, b = a + width;
  const fields = s.fields.filter((f) => f.kind !== "time");
  const q = `SELECT epoch_us(ts) - ${run.t0} AS t${fields.map((f) => `, ${ident(f.name)}`).join("")} FROM ${ident(s.site)}
    WHERE ts > ${at(a)} AND ts < ${at(b + s.span)} AND epoch_us(ts) - (${spanSql(s)}) < ${b} AND (${spanSql(s)}) >= ${cutoff}`;
  const r = await sql(run.project, run.id, q, 100_000, signal);
  const items = r.rows.map((x) => ({
    t: n(x[0]),
    v: Object.fromEntries(fields.map((f, i) => [f.name, f.kind === "number" ? n(x[i + 1]) : (x[i + 1] as string | boolean)])),
  }));
  return { items, truncated: r.truncated };
}

/** A site's rows as spans for the flame chart: each row ending at its time, as long as its enabled
 * durations, those laid end to end beneath it in field order; lanes by the `split` label. */
export function rowZones(s: Site, rows: Row[], on: boolean[], split: string): { zones: Zone[]; lanes: string[] } {
  const zones: Zone[] = [];
  for (const r of rows) {
    const parts = s.durations.map((d, k) => (on[k] ? Math.max(0, Number(r.v[d]) * s.scale[k] || 0) : 0));
    const total = parts.reduce((x, v) => x + v, 0);
    if (total <= 0) continue;
    const lane = split ? `${split}=${r.v[split]}` : s.site;
    const start = r.t - total;
    const row = zones.length;
    zones.push({ lane, name: s.site, category: "row", t: start, dur: total, depth: 0, parent: -1 });
    let x = start;
    parts.forEach((v, k) => {
      if (v <= 0) return;
      zones.push({ lane, name: s.durations[k], category: "row", t: x, dur: v, depth: 1, parent: row });
      x += v;
    });
  }
  return { zones, lanes: [...new Set(zones.map((z) => z.lane))].sort() };
}

/** Depth and parent of each zone on its lane, by containment: the flame chart's rows. */
export function nest(zones: Zone[]) {
  const order = zones.map((_, i) => i).sort((a, b) => zones[a].lane.localeCompare(zones[b].lane) || zones[a].t - zones[b].t || zones[b].dur - zones[a].dur);
  let lane = "";
  const stack: number[] = [];
  for (const i of order) {
    const z = zones[i];
    if (z.lane !== lane) { lane = z.lane; stack.length = 0; }
    while (stack.length && zones[stack[stack.length - 1]].t + zones[stack[stack.length - 1]].dur <= z.t) stack.pop();
    z.depth = stack.length;
    z.parent = stack.length ? stack[stack.length - 1] : -1;
    stack.push(i);
  }
}
