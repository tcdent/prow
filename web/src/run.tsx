// One run: its identity, the overview and flame chart over a view range, and below them the view's
// flame graph, zone statistics, plots and each site's rows. The range and the panel ride in the URL.

import { useCallback, useEffect, useMemo, useState } from "react";
import { loadRun, type Run as RunData, type RunEntry, type Table } from "./api";
import { Histogram, TimeChart, type Point } from "./charts";
import { FlameGraph } from "./flamegraph";
import { dur, num, quantile, wall } from "./fmt";
import { FlameChart, Overview, type View } from "./timeline";

interface Props { entry: RunEntry; params: URLSearchParams; setParams: (p: Record<string, string | null>) => void }

export function RunPage({ entry, params, setParams }: Props) {
  const [run, setRun] = useState<RunData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [focus, setFocus] = useState<string | null>(null);
  const runId = String(entry.manifest.run);
  useEffect(() => {
    setRun(null);
    loadRun(entry).then(setRun, (e) => setError(String(e)));
  }, [runId]); // eslint-disable-line react-hooks/exhaustive-deps

  if (error) return <div className="empty">{error}</div>;
  if (!run) return <div className="empty">reading {Object.values(entry.sites).flat().length} files…</div>;
  return <Loaded run={run} params={params} setParams={setParams} focus={focus} setFocus={setFocus} />;
}

function Loaded({ run, params, setParams, focus, setFocus }: { run: RunData; params: URLSearchParams; setParams: Props["setParams"]; focus: string | null; setFocus: (f: string | null) => void }) {
  const view: View = useMemo(() => {
    const f = Number(params.get("f")), t = Number(params.get("t"));
    return params.has("f") && t > f ? { from: Math.max(0, f), to: Math.min(run.end, t) } : { from: 0, to: run.end };
  }, [params, run.end]);
  const setView = useCallback(
    (v: View) => {
      const whole = v.from <= 0 && v.to >= run.end;
      setParams(whole ? { f: null, t: null } : { f: v.from.toFixed(1), t: v.to.toFixed(1) });
    },
    [run.end, setParams],
  );
  const zoomOut = () => {
    const c = (view.from + view.to) / 2, half = view.to - view.from;
    setView({ from: Math.max(0, c - half), to: Math.min(run.end, c + half) });
  };
  const panels = ["flame graph", "zones", ...(run.plots ? ["plots"] : []), ...run.sites.map((s) => s.site)];
  const panel = panels.includes(params.get("p") ?? "") ? params.get("p")! : panels[0];
  const m = run.entry.manifest;

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
        <span>view +{dur(view.from)} → +{dur(view.to)}</span>
        <span className="dim">{dur(view.to - view.from)}</span>
        <button onClick={zoomOut}>zoom out</button>
        <button onClick={() => setView({ from: 0, to: run.end })}>whole run</button>
        <span className="dim hint">drag to select · wheel to zoom · click a zone to fit it · double-click for the run</span>
      </div>
      <Overview run={run} view={view} setView={setView} />
      {run.zones.length > 0 ? <FlameChart run={run} view={view} setView={setView} focus={focus} /> : <div className="empty">no zones in this run</div>}

      <nav className="tabs">
        {panels.map((p) => (
          <button key={p} className={p === panel ? "on" : ""} onClick={() => setParams({ p })}>{p}</button>
        ))}
        {focus && <button className="focus" onClick={() => setFocus(null)}>highlight: {focus} ✕</button>}
      </nav>
      {panel === "flame graph" && <FlameGraph run={run} view={view} onPick={setFocus} />}
      {panel === "zones" && <ZoneTable run={run} view={view} focus={focus} setFocus={setFocus} />}
      {panel === "plots" && run.plots && <Plots table={run.plots} view={view} setView={setView} />}
      {run.sites.filter((s) => s.site === panel).map((s) => <SitePanel key={s.site} table={s} view={view} setView={setView} params={params} setParams={setParams} />)}
    </div>
  );
}

function ZoneTable({ run, view, focus, setFocus }: { run: RunData; view: View; focus: string | null; setFocus: (f: string | null) => void }) {
  const rows = useMemo(() => {
    const by = new Map<string, { lane: string; name: string; durs: number[]; total: number }>();
    for (const z of run.zones) {
      const clipped = Math.min(z.t + z.dur, view.to) - Math.max(z.t, view.from);
      if (clipped <= 0) continue;
      const k = `${z.lane}\u0000${z.name}`;
      if (!by.has(k)) by.set(k, { lane: z.lane, name: z.name, durs: [], total: 0 });
      const r = by.get(k)!;
      r.durs.push(z.dur);
      r.total += clipped;
    }
    return [...by.values()].map((r) => ({ ...r, durs: r.durs.sort((a, b) => a - b) })).sort((a, b) => b.total - a.total);
  }, [run, view]);
  const span = view.to - view.from;
  return (
    <table className="stats">
      <thead>
        <tr><th>lane</th><th>zone</th><th>count</th><th>in view</th><th>share</th><th>mean</th><th>p50</th><th>p95</th><th>max</th></tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.lane + r.name} className={focus === r.name ? "on" : ""} onClick={() => setFocus(focus === r.name ? null : r.name)}>
            <td className="dim">{r.lane}</td>
            <td>{r.name}</td>
            <td className="num">{r.durs.length}</td>
            <td className="num">{dur(r.total)}</td>
            <td className="num">{((r.total / span) * 100).toFixed(1)}%</td>
            <td className="num">{dur(r.durs.reduce((s, d) => s + d, 0) / r.durs.length)}</td>
            <td className="num">{dur(quantile(r.durs, 0.5))}</td>
            <td className="num">{dur(quantile(r.durs, 0.95))}</td>
            <td className="num">{dur(r.durs[r.durs.length - 1])}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function Plots({ table, view, setView }: { table: Table; view: View; setView: (v: View) => void }) {
  const byName = useMemo(() => {
    const m = new Map<string, Point[]>();
    for (let i = 0; i < table.t.length; i++) {
      const n = table.cols.name[i] as string;
      if (!m.has(n)) m.set(n, []);
      m.get(n)!.push({ t: table.t[i], v: table.cols.value[i] as number, g: "" });
    }
    return [...m.entries()].sort(([a], [b]) => a.localeCompare(b));
  }, [table]);
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

function SitePanel({ table, view, setView, params, setParams }: { table: Table; view: View; setView: (v: View) => void; params: URLSearchParams; setParams: Props["setParams"] }) {
  const numeric = table.fields.filter((f) => f.kind === "number").map((f) => f.name);
  const labels = table.fields.filter((f) => f.kind === "label" || f.kind === "bool").map((f) => f.name);
  // a field named like a duration reads as one: ms, us, ns, *_ms, *_us, *_ns
  const pickY = params.get("y");
  const y = pickY && numeric.includes(pickY) ? pickY : numeric.find((n) => /(^|_)(ms|us|ns)$/.test(n)) ?? numeric[0];
  const pickC = params.get("c");
  const c = pickC === "" ? "" : pickC && labels.includes(pickC) ? pickC : labels[0] ?? "";
  const chart = params.get("k") === "histogram" ? "histogram" : "time";
  const unit = y && /(^|_)ms$/.test(y) ? (v: number) => dur(v * 1e3) : y && /(^|_)us$/.test(y) ? dur : y && /(^|_)ns$/.test(y) ? (v: number) => dur(v / 1e3) : undefined;

  const points = useMemo(() => {
    if (!y) return [];
    const out: Point[] = [];
    for (let i = 0; i < table.t.length; i++) out.push({ t: table.t[i], v: table.cols[y][i] as number, g: c ? String(table.cols[c][i]) : "" });
    return out;
  }, [table, y, c]);
  const inView = useMemo(() => points.filter((p) => p.t >= view.from && p.t <= view.to), [points, view]);
  const groups = useMemo(() => {
    const m = new Map<string, number[]>();
    for (const p of inView) {
      if (!m.has(p.g)) m.set(p.g, []);
      m.get(p.g)!.push(p.v);
    }
    return [...m.entries()].map(([g, vs]) => ({ g, vs: vs.sort((a, b) => a - b) })).sort((a, b) => b.vs.length - a.vs.length);
  }, [inView]);
  const f = unit ?? num;

  if (!y) return <div className="empty">no numeric fields at this site</div>;
  return (
    <div className="site">
      <div className="controls">
        <label>value <select value={y} onChange={(e) => setParams({ y: e.target.value })}>{numeric.map((n) => <option key={n}>{n}</option>)}</select></label>
        <label>colour by <select value={c} onChange={(e) => setParams({ c: e.target.value })}><option value="">—</option>{labels.map((n) => <option key={n}>{n}</option>)}</select></label>
        <span className="ranges">
          <button className={chart === "time" ? "on" : ""} onClick={() => setParams({ k: null })}>over time</button>
          <button className={chart === "histogram" ? "on" : ""} onClick={() => setParams({ k: "histogram" })}>histogram</button>
        </span>
        <span className="dim">{inView.length} of {points.length} rows in view</span>
      </div>
      <div className="chart">
        {chart === "time" ? <TimeChart points={points} view={view} setView={setView} mode="dots" unit={unit} /> : <Histogram points={inView} unit={unit} />}
      </div>
      <table className="stats">
        <thead>
          <tr><th>{c || "rows"}</th><th>count</th><th>mean</th><th>p50</th><th>p90</th><th>p99</th><th>min</th><th>max</th></tr>
        </thead>
        <tbody>
          {groups.map(({ g, vs }) => (
            <tr key={g}>
              <td>{g || "all"}</td>
              <td className="num">{vs.length}</td>
              <td className="num">{f(vs.reduce((s, v) => s + v, 0) / vs.length)}</td>
              <td className="num">{f(quantile(vs, 0.5))}</td>
              <td className="num">{f(quantile(vs, 0.9))}</td>
              <td className="num">{f(quantile(vs, 0.99))}</td>
              <td className="num">{f(vs[0])}</td>
              <td className="num">{f(vs[vs.length - 1])}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
