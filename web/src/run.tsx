// One run: its identity, the overview and the flame chart over a view range, then every section down
// the page (the flame graph, the zones, each site, plots, messages). Everything shown is a query to
// the server: a summary up front, time tiles for the charts, one aggregate a table over the view.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { loadRun, nest, rowZones, siteTile, zoneTile, type Row, type Run as RunData, type RunEntry, type Site, type Zone } from "./api";
import { BINS, Histogram, TimeChart, type Bins, type Point } from "./charts";
import { FlameGraph } from "./flamegraph";
import { dur, durationScale, isDuration, layerColors, num, wall, GRAY } from "./fmt";
import { at, ident, n, objects, sql, useQuery } from "./sql";
import { Layers, StackTotals, type GroupMeans, type LayerStats } from "./stack";
import { useTiles } from "./tiles";
import { FlameChart, Overview, type Density, type View } from "./timeline";

interface Props { project: string; entry: RunEntry; params: URLSearchParams; setParams: (p: Record<string, string | null>) => void; reload: () => void }

export function RunPage({ project, entry, params, setParams, reload }: Props) {
  const [run, setRun] = useState<RunData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [focus, setFocus] = useState<string | null>(null);
  const runId = String(entry.manifest.run);
  // a live run's index entry grows by parts: summarize it again when it does, keeping the page
  const files = Object.values(entry.sites).flat().length;
  useEffect(() => {
    loadRun(project, entry).then(setRun, (e) => setError(String(e)));
  }, [runId, files]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => setRun(null), [runId]);

  if (error) return <div className="empty">{error}</div>;
  if (!run) return <div className="empty">reading the run…</div>;
  return <Loaded key={`${runId}:${files}`} run={run} params={params} setParams={setParams} focus={focus} setFocus={setFocus} reload={reload} />;
}

function Loaded({ run, params, setParams, focus, setFocus, reload }: { run: RunData; params: URLSearchParams; setParams: Props["setParams"]; focus: string | null; setFocus: (f: string | null) => void; reload: () => void }) {
  const sites = [...run.sites].sort((a, b) => (a.site === "step" ? -1 : b.site === "step" ? 1 : a.site.localeCompare(b.site)));
  // Times stay µs from the run's start. The run's activity starts where its busiest source (the
  // zones, or the site with the most rows) begins: the view opens there and the overview starts
  // there, not on the air before it (a long boot). Viewing earlier widens the overview to match.
  const start = useMemo(() => activityStart(run), [run]);
  // the whole run, as the page opens on it and as the whole-run button and zooming out reach it
  const whole: View = useMemo(() => ({ from: Math.max(0, start - (run.end - start) * 0.02), to: run.end }), [start, run.end]);
  const urlView: View = useMemo(() => {
    const f = Number(params.get("f")), t = Number(params.get("t"));
    if (params.has("f") && t > f) return { from: Math.max(0, f), to: Math.min(run.end, t) };
    return whole;
  }, [params, run.end, whole]);
  // The view lives here and reaches the URL once it settles (400 ms after the last change): Chrome
  // drops history updates that come faster than about 200 in 10 s, and a wheel sends 60 a second,
  // which froze the view for seconds at a time. A range from elsewhere (back, a link) takes over.
  const [live, setLive] = useState<View | null>(null);
  const written = useRef("");
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const urlKey = `${params.get("f")}:${params.get("t")}`;
  useEffect(() => {
    if (urlKey !== written.current) setLive(null);
  }, [urlKey]);
  useEffect(() => () => clearTimeout(timer.current), []);
  const view = live ?? urlView;
  const setView = useCallback(
    (v: View) => {
      setLive(v);
      clearTimeout(timer.current);
      timer.current = setTimeout(() => {
        const f = v.from.toFixed(1), t = v.to.toFixed(1);
        written.current = `${f}:${t}`;
        setParams({ f, t });
      }, 400);
    },
    [setParams],
  );
  const zoomOut = () => {
    const c = (view.from + view.to) / 2, half = view.to - view.from;
    setView({ from: Math.max(whole.from, c - half), to: Math.min(whole.to, c + half) });
  };
  const m = run.entry.manifest;
  const overviewStart = Math.min(whole.from, view.from);
  const density = useDensity(run, overviewStart, sites);

  // the zones of the view, by tiles
  const zoneTiles = useTiles<Zone>(
    `zones:${run.id}`,
    view,
    run.end,
    (s, w, c, signal) => zoneTile(run, s, w, c, signal),
    (z) => `${z.lane}|${z.name}|${z.t}|${z.dur}`,
  );
  const zones = useMemo(() => {
    const zs = zoneTiles.items.map((z) => ({ ...z }));
    nest(zs);
    return zs;
  }, [zoneTiles.items]);

  return (
    <div className="run">
      <h1>
        {String(m.workload ?? m.project)} <span className="dim">{String(m.run).slice(0, 12)}</span>
      </h1>
      <div className="facts">
        <span className={m.state === "complete" ? "" : "warn"}>{String(m.state)}</span>
        <span>{String(m.host)}</span>
        <span>{String(m["build.commit"] ?? "")}{m["build.dirty"] ? "*" : ""}</span>
        <span>{wall(run.t0, 0)}</span>
        <span>{dur(run.end)}</span>
        <span className={m["clock.synced"] ? "" : "warn"}>clock {m["clock.synced"] ? "synced" : "unsynced"} ±{dur(Number(m["clock.maxerror_us"] ?? 0))}</span>
      </div>
      <details className="attrs">
        <summary>all attributes</summary>
        <table className="attrs">
          <tbody>
            {Object.entries(m).sort(([a], [b]) => a.localeCompare(b)).map(([k, v]) => (
              <tr key={k}><td className="dim">{k}</td><td>{String(v)}</td></tr>
            ))}
          </tbody>
        </table>
      </details>

      <div className="viewbar">
        <span>+{dur(view.from)} → +{dur(view.to)}</span>
        <span className="dim">{dur(view.to - view.from)}</span>
        <button className="icon" onClick={zoomOut} title="zoom out">{ICON.zoomOut}</button>
        <button className="icon" onClick={() => setView(whole)} title="whole run">{ICON.whole}</button>
        {m.state !== "complete" && <button className="icon" onClick={reload} title="reload: read the parts written since this page loaded">{ICON.reload}</button>}
        {focus && <button className="focus" onClick={() => setFocus(null)}>highlight: {focus} ✕</button>}
      </div>
      <Overview rows={density ?? []} marks={run.marks} start={overviewStart} end={run.end} view={view} setView={setView} />
      {run.zones && <FlameChart chart={{ zones, lanes: run.zones.lanes, end: run.end, marks: run.marks }} view={view} setView={setView} focus={focus} />}

      {run.zones && (
        <>
          <Section title="flame graph"><FlameGraph run={run} view={view} onPick={setFocus} /></Section>
          <Section title="zones" sub={`${run.zones.n} zones`}><ZoneTable run={run} view={view} focus={focus} setFocus={setFocus} /></Section>
        </>
      )}
      {sites.map((s) => (
        <Section key={s.site} title={s.site} sub={`${s.n} rows`}>
          <SitePanel run={run} site={s} view={view} setView={setView} params={params} setParams={setParams} />
        </Section>
      ))}
      {run.plots.length > 0 && <Section title="plots"><Plots run={run} view={view} setView={setView} /></Section>}
      {run.marks.length > 0 && <Section title="messages"><Messages run={run} view={view} /></Section>}
    </div>
  );
}

/**
 * A section of the page. Its content is live only near the viewport (600 px either side): off
 * screen it is a placeholder of its last height, so its queries, tiles and redraws stop until it
 * is scrolled back to.
 */
function Section({ title, sub, children }: { title: string; sub?: string; children: React.ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  const [near, setNear] = useState(false);
  const height = useRef(160);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const io = new IntersectionObserver(([e]) => {
      if (!e.isIntersecting && el.offsetHeight > 0) height.current = el.offsetHeight;
      setNear(e.isIntersecting);
    }, { rootMargin: "600px 0px" });
    io.observe(el);
    return () => io.disconnect();
  }, []);
  return (
    <section className="block">
      <h2>{title} {sub && <span className="dim">{sub}</span>}</h2>
      <div ref={ref} style={near ? undefined : { minHeight: height.current }}>{near && children}</div>
    </section>
  );
}

/** Small line icons, in the text color. */
const svg = (d: React.ReactNode) => (
  <svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">{d}</svg>
);
const ICON = {
  // a magnifier with a minus
  zoomOut: svg(<><circle cx="6.5" cy="6.5" r="4.5" /><path d="M4.5 6.5h4M10 10l4.5 4.5" /></>),
  // arrows out to both edges
  whole: svg(<><path d="M1.5 8h13M1.5 8l3-3M1.5 8l3 3M14.5 8l-3-3M14.5 8l-3 3" /><path d="M1 2.5v11M15 2.5v11" /></>),
  // a circular arrow
  reload: svg(<><path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" /><path d="M13.5 2.5v3h-3" /></>),
};

/** Where the run's activity starts (µs): the first event of its busiest source, the zones or a site. */
function activityStart(run: RunData): number {
  const sources: { n: number; first: number }[] = run.sites.map((s) => ({ n: s.n, first: s.first }));
  if (run.zones) sources.push({ n: run.zones.n, first: run.zones.first });
  if (!sources.length) return 0;
  return Math.max(0, sources.sort((a, b) => b.n - a.n)[0].first);
}

const DENSITY_BUCKETS = 1000;

/** The overview's rows: each zone lane's and each site's events in equal buckets from `start` to the end. */
function useDensity(run: RunData, start: number, sites: Site[]): Density[] | null {
  const b = Math.max((run.end - start) / DENSITY_BUCKETS, 1);
  const bucket = `floor((epoch_us(ts) - ${run.t0 + start}) / ${b})::BIGINT`;
  const parts: string[] = [];
  if (run.zones) parts.push(`SELECT lane AS label, ${bucket} AS b, count(*) AS c FROM zones WHERE ts >= ${at(run.t0 + start)} GROUP BY ALL`);
  for (const s of sites) parts.push(`SELECT ${`'${s.site}'`} AS label, ${bucket} AS b, count(*) AS c FROM ${ident(s.site)} WHERE ts >= ${at(run.t0 + start)} GROUP BY ALL`);
  const q = parts.join(" UNION ALL ");
  const { data } = useQuery(parts.length ? `${run.id}:${start.toFixed(0)}` : null, (signal) => sql(run.project, run.id, q, 200_000, signal), 0);
  return useMemo(() => {
    if (!data) return parts.length ? null : [];
    const labels = [...(run.zones?.lanes ?? []), ...sites.map((s) => s.site)];
    const rows = new Map(labels.map((l) => [l, new Array(DENSITY_BUCKETS).fill(0) as number[]]));
    for (const r of data.rows) {
      const k = n(r[1]);
      const row = rows.get(String(r[0]));
      if (row && k >= 0 && k < DENSITY_BUCKETS) row[k] += n(r[2]);
    }
    return labels.map((label) => ({ label, bins: rows.get(label)! }));
  }, [data]); // eslint-disable-line react-hooks/exhaustive-deps
}

/** A SQL condition: rows whose time falls in the view. */
const inView = (run: RunData, v: View) => `ts >= ${at(run.t0 + v.from)} AND ts <= ${at(run.t0 + v.to)}`;
const viewKey = (v: View) => `${v.from.toFixed(0)}:${v.to.toFixed(0)}`;

function Messages({ run, view }: { run: RunData; view: View }) {
  const { data } = useQuery(`msg:${viewKey(view)}`, (signal) =>
    sql(run.project, run.id, `SELECT epoch_us(ts) - ${run.t0} AS t, category, text FROM messages WHERE ${inView(run, view)} ORDER BY ts`, 2000, signal),
  );
  const rows = data ? objects(data) : [];
  return (
    <table className="stats">
      <thead><tr><th>at</th><th>category</th><th>text</th></tr></thead>
      <tbody>
        {rows.map((r, i) => (
          <tr key={i}><td className="dim">+{dur(n(r.t))}</td><td className="dim">{String(r.category)}</td><td>{String(r.text)}</td></tr>
        ))}
      </tbody>
    </table>
  );
}

function ZoneTable({ run, view, focus, setFocus }: { run: RunData; view: View; focus: string | null; setFocus: (f: string | null) => void }) {
  const longest = run.zones?.longest ?? 0;
  const q = `SELECT lane, name, count(*) AS n, sum(least(e, ${view.to}) - greatest(s, ${view.from})) AS inview,
      avg(d) AS mean, quantile_cont(d, 0.5) AS p50, quantile_cont(d, 0.95) AS p95, max(d) AS mx
    FROM (SELECT lane, name, epoch_us(ts) - ${run.t0} AS s, dur_ns / 1000.0 AS d, epoch_us(ts) - ${run.t0} + dur_ns / 1000.0 AS e FROM zones
          WHERE ts >= ${at(run.t0 + view.from - longest)} AND ts < ${at(run.t0 + view.to)})
    WHERE e > ${view.from} GROUP BY ALL ORDER BY inview DESC LIMIT 500`;
  const { data } = useQuery(`zt:${viewKey(view)}`, (signal) => sql(run.project, run.id, q, 500, signal));
  const rows = data ? objects(data) : [];
  const span = view.to - view.from;
  return (
    <table className="stats">
      <thead>
        <tr><th>lane</th><th>zone</th><th>count</th><th>in view</th><th>share</th><th>mean</th><th>p50</th><th>p95</th><th>max</th></tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={`${r.lane}|${r.name}`} className={focus === r.name ? "on" : ""} onClick={() => setFocus(focus === r.name ? null : String(r.name))}>
            <td className="dim">{String(r.lane)}</td>
            <td>{String(r.name)}</td>
            <td className="num">{num(n(r.n))}</td>
            <td className="num">{dur(n(r.inview))}</td>
            <td className="num">{((n(r.inview) / span) * 100).toFixed(1)}%</td>
            <td className="num">{dur(n(r.mean))}</td>
            <td className="num">{dur(n(r.p50))}</td>
            <td className="num">{dur(n(r.p95))}</td>
            <td className="num">{dur(n(r.mx))}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** A site with at most this many rows loads whole at any zoom (no tile cutoff). */
const SMALL_SITE = 20_000;

/** Points a pixel or so apart: the view in this many buckets, the mean of each. */
const POINT_BUCKETS = 800;

function Plots({ run, view, setView }: { run: RunData; view: View; setView: (v: View) => void }) {
  const b = Math.max((view.to - view.from) / POINT_BUCKETS, 1);
  const q = `SELECT name, floor((epoch_us(ts) - ${run.t0 + view.from}) / ${b})::BIGINT AS k, avg(value) AS v, arg_max(value, ts) AS last
    FROM plots WHERE ${inView(run, view)} GROUP BY ALL ORDER BY name, k`;
  const { data } = useQuery(`plots:${viewKey(view)}`, (signal) => sql(run.project, run.id, q, 200_000, signal));
  const byName = useMemo(() => {
    const m = new Map<string, Point[]>();
    for (const r of data ? objects(data) : []) {
      const name = String(r.name);
      if (!m.has(name)) m.set(name, []);
      m.get(name)!.push({ t: view.from + (n(r.k) + 0.5) * b, v: n(r.v), g: "" });
    }
    return [...m.entries()];
  }, [data]); // eslint-disable-line react-hooks/exhaustive-deps
  return (
    <div className="grid2">
      {byName.map(([name, pts]) => (
        <div className="chart" key={name}>
          <div className="head"><span className="title">{name}</span><span className="now">{num(pts[pts.length - 1].v)}</span></div>
          <TimeChart points={pts} view={view} setView={setView} mode="line" />
        </div>
      ))}
    </div>
  );
}

function SitePanel({ run, site: s, view, setView, params, setParams }: { run: RunData; site: Site; view: View; setView: (v: View) => void; params: URLSearchParams; setParams: Props["setParams"] }) {
  // every site keeps its own settings in the URL: <key>.<site>
  const key = (k: string) => `${k}.${s.site}`;
  const numeric = s.fields.filter((f) => f.kind === "number").map((f) => f.name);
  const labels = s.fields.filter((f) => f.kind === "label" || f.kind === "bool").map((f) => f.name);
  const durations = s.durations;
  const off = new Set((params.get(key("off")) ?? "").split(",").filter(Boolean));
  const on = durations.map((d) => !off.has(d));
  const setOn = (next: boolean[]) => setParams({ [key("off")]: durations.filter((_, i) => !next[i]).join(",") || null });

  const mode = params.get(key("k")) === "histogram" ? "histogram" : params.get(key("k")) === "value" || !durations.length ? "value" : "stack";
  const pickC = params.get(key("c"));
  const c = pickC === "" ? "" : pickC && labels.includes(pickC) ? pickC : labels[0] ?? "";
  const g = c ? `${ident(c)}::VARCHAR` : "''";

  // totals by layer over the view: they also order the layers' colors
  const stat = (d: string, i: number) => `${ident(d)} * ${s.scale[i]}`;
  const layerQ = durations.length
    ? `SELECT count(*) AS n${durations.map((d, i) => `, sum(${stat(d, i)}), avg(${stat(d, i)}), quantile_cont(${stat(d, i)}, 0.5), quantile_cont(${stat(d, i)}, 0.9), max(${stat(d, i)})`).join("")} FROM ${ident(s.site)} WHERE ${inView(run, view)}`
    : null;
  const layerData = useQuery(layerQ && mode === "stack" ? `lt:${s.site}:${viewKey(view)}` : null, (signal) => sql(run.project, run.id, layerQ!, 10, signal));
  const stats: LayerStats[] = useMemo(() => {
    const row = layerData.data?.rows[0];
    if (!row) return [];
    return durations.map((_, i) => ({ i, total: n(row[1 + i * 5]) || 0, mean: n(row[2 + i * 5]) || 0, p50: n(row[3 + i * 5]) || 0, p90: n(row[4 + i * 5]) || 0, max: n(row[5 + i * 5]) || 0 }));
  }, [layerData.data]); // eslint-disable-line react-hooks/exhaustive-deps
  const inViewCount = n(layerData.data?.rows[0]?.[0]);
  const groupQ = durations.length
    ? `SELECT ${g} AS g, count(*) AS n${durations.map((d, i) => `, avg(${stat(d, i)})`).join("")} FROM ${ident(s.site)} WHERE ${inView(run, view)} GROUP BY 1 ORDER BY 1`
    : null;
  const groupData = useQuery(groupQ && mode === "stack" ? `gm:${s.site}:${c}:${viewKey(view)}` : null, (signal) => sql(run.project, run.id, groupQ!, 1000, signal));
  const groups: GroupMeans[] = (groupData.data?.rows ?? []).map((r) => ({ g: String(r[0]), n: n(r[1]), mean: durations.map((_, i) => n(r[2 + i]) || 0) }));
  // colors by each layer's weight over the whole site, fixed for the page
  const weights = useQuery(durations.length ? `w:${s.site}` : null, (signal) =>
    sql(run.project, run.id, `SELECT ${durations.map((d, i) => `sum(${stat(d, i)})`).join(", ")} FROM ${ident(s.site)}`, 1, signal),
  0);
  const colors = useMemo(() => layerColors(durations.map((_, i) => n(weights.data?.rows[0]?.[i]) || 0)), [weights.data]); // eslint-disable-line react-hooks/exhaustive-deps
  const colorFor = useCallback((name: string) => {
    const k = durations.indexOf(name);
    return k < 0 ? GRAY : colors[k];
  }, [colors]); // eslint-disable-line react-hooks/exhaustive-deps

  // the rows of the view as spans, by tiles
  const tiles = useTiles<Row>(
    `site:${run.id}:${s.site}`,
    view,
    run.end,
    // the cutoff bounds a large site's rows a tile; a small site comes whole at any zoom
    (start, w, cut, signal) => siteTile(run, s, start, w, s.n > SMALL_SITE ? cut : 0, signal),
    (r) => `${r.t}`,
    mode === "stack",
  );
  const chart = useMemo(() => {
    const { zones, lanes } = rowZones(s, tiles.items, on, c);
    return { zones, lanes, end: run.end, marks: run.marks };
  }, [tiles.items, on.join(), c]); // eslint-disable-line react-hooks/exhaustive-deps

  // one value: its mean a bucket, by group; its histogram; its figures by group
  const pickY = params.get(key("y"));
  const y = pickY && numeric.includes(pickY) ? pickY : durations[0] ?? numeric[0];
  const yScale = y && isDuration(y) ? durationScale(y) : 1;
  const unit = y && isDuration(y) ? (v: number) => dur(v) : undefined;
  const b = Math.max((view.to - view.from) / POINT_BUCKETS, 1);
  const yv = y ? `${ident(y)} * ${yScale}` : "0";
  const pointsData = useQuery(y && mode === "value" ? `pv:${s.site}:${y}:${c}:${viewKey(view)}` : null, (signal) =>
    sql(run.project, run.id, `SELECT ${g} AS g, floor((epoch_us(ts) - ${run.t0 + view.from}) / ${b})::BIGINT AS k, avg(${yv}) AS v FROM ${ident(s.site)} WHERE ${inView(run, view)} AND ${ident(y!)} IS NOT NULL GROUP BY ALL`, 200_000, signal),
  );
  const points: Point[] = (pointsData.data?.rows ?? []).map((r) => ({ t: view.from + (n(r[1]) + 0.5) * b, v: n(r[2]), g: String(r[0]) }));
  const histData = useQuery(y && mode === "histogram" ? `h:${s.site}:${y}:${c}:${viewKey(view)}` : null, (signal) =>
    sql(run.project, run.id, `WITH v AS (SELECT ${g} AS g, ${yv} AS y FROM ${ident(s.site)} WHERE ${inView(run, view)} AND ${ident(y!)} IS NOT NULL),
      r AS (SELECT min(y) AS lo, max(y) AS hi FROM v)
      SELECT g, least(${BINS - 1}, floor((y - lo) / nullif(hi - lo, 0) * ${BINS}))::BIGINT AS k, count(*) AS c, any_value(lo), any_value(hi) FROM v, r GROUP BY ALL`, 100_000, signal),
  );
  const bins: Bins = useMemo(() => {
    const rows = histData.data?.rows ?? [];
    const groups = [...new Set(rows.map((r) => String(r[0])))].sort();
    const counts = groups.map(() => new Array(BINS).fill(0) as number[]);
    for (const r of rows) counts[groups.indexOf(String(r[0]))][n(r[1]) || 0] += n(r[2]);
    return { lo: n(rows[0]?.[3]) || 0, hi: n(rows[0]?.[4]) || 1, groups, counts };
  }, [histData.data]);
  const figQ = y ? `SELECT ${g} AS g, count(*) AS n, avg(${yv}), quantile_cont(${yv}, 0.5), quantile_cont(${yv}, 0.9), quantile_cont(${yv}, 0.99), min(${yv}), max(${yv}) FROM ${ident(s.site)} WHERE ${inView(run, view)} AND ${ident(y)} IS NOT NULL GROUP BY 1 ORDER BY 2 DESC` : null;
  const figData = useQuery(figQ && mode !== "stack" ? `fig:${s.site}:${y}:${c}:${viewKey(view)}` : null, (signal) => sql(run.project, run.id, figQ!, 1000, signal));
  const f = unit ?? num;

  if (!y) return <div className="empty">no numeric fields at this site</div>;
  return (
    <div className="site">
      <div className="controls">
        <span className="ranges">
          {durations.length > 0 && <button className={mode === "stack" ? "on" : ""} onClick={() => setParams({ [key("k")]: null })}>flame chart</button>}
          <button className={mode === "value" ? "on" : ""} onClick={() => setParams({ [key("k")]: "value" })}>one value</button>
          <button className={mode === "histogram" ? "on" : ""} onClick={() => setParams({ [key("k")]: "histogram" })}>histogram</button>
        </span>
        {mode === "stack" ? (
          <Layers layers={durations} colors={colors} on={on} setOn={setOn} />
        ) : (
          <label>value <select value={y} onChange={(e) => setParams({ [key("y")]: e.target.value })}>{numeric.map((x) => <option key={x}>{x}</option>)}</select></label>
        )}
        <label>{mode === "stack" ? "lanes by" : "color by"} <select value={c} onChange={(e) => setParams({ [key("c")]: e.target.value })}><option value="">—</option>{labels.map((x) => <option key={x}>{x}</option>)}</select></label>
        {mode === "stack" && <span className="dim">{Number.isFinite(inViewCount) ? `${num(inViewCount)} of ${num(s.n)} rows in view` : ""}{tiles.truncated ? " · zoom in for every row" : ""}</span>}
      </div>
      {mode === "stack" ? (
        <>
          {chart.zones.length > 0 ? (
            <FlameChart chart={chart} view={view} setView={setView} focus={null} color={colorFor} />
          ) : (
            <div className="empty">{tiles.loading ? "loading…" : inViewCount > 0 ? "rows under a pixel wide at this zoom" : "no rows in the view"}</div>
          )}
          <StackTotals layers={durations} colors={colors} on={on} setOn={setOn} groups={groups} stats={stats} />
        </>
      ) : (
        <>
          <div className="chart">
            {mode === "value" ? <TimeChart points={points} view={view} setView={setView} mode="dots" unit={unit} /> : <Histogram data={bins} unit={unit} />}
          </div>
          <table className="stats">
            <thead>
              <tr><th>{c || "rows"}</th><th>count</th><th>mean</th><th>p50</th><th>p90</th><th>p99</th><th>min</th><th>max</th></tr>
            </thead>
            <tbody>
              {(figData.data?.rows ?? []).map((r) => (
                <tr key={String(r[0])}>
                  <td>{String(r[0]) || "all"}</td>
                  <td className="num">{num(n(r[1]))}</td>
                  {[2, 3, 4, 5, 6, 7].map((i) => <td key={i} className="num">{f(n(r[i]))}</td>)}
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
    </div>
  );
}
