// The run over time: an overview of the whole run with the view's range on it, and the flame chart
// of the view: one band a lane, one row a nesting depth, zones as bars.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Zone } from "./api";
import { useCanvas, useDrag, useWidth } from "./canvas";
import { colorOf, css, dur, tickLabel, ticks, GRAY, type RGB } from "./fmt";

export interface View { from: number; to: number }

const LABEL = 92; // the lane-name column, CSS px
const ROW = 15;
const GAP = 8;
const AXIS = 18;

const toX = (t: number, v: View, w: number) => ((t - v.from) / (v.to - v.from)) * w;
const toT = (x: number, v: View, w: number) => v.from + (x / w) * (v.to - v.from);

/** A row of the overview: counts in equal buckets over [start, end]. */
export interface Density { label: string; bins: number[] }

/** The run, small, from `start` (its activity, or earlier when the view is) to `end`: each lane's
 * and site's events a pixel, messages as ticks, the view outlined. */
export function Overview({ rows, marks, start, end, view, setView }: { rows: Density[]; marks: number[]; start: number; end: number; view: View; setView: (v: View) => void }) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const w = useWidth(box);
  const pw = Math.max(w - LABEL, 1);
  const h = rows.length * 12 + 14;
  const all: View = useMemo(() => ({ from: start, to: end }), [start, end]);

  // each row's buckets onto pixels (the busiest bucket a pixel), as a share of the row's peak
  const px = useMemo(
    () =>
      rows.map((r) => {
        const out = new Float64Array(pw);
        const nb = r.bins.length;
        for (let i = 0; i < nb; i++) {
          const p = Math.min(pw - 1, Math.floor((i / nb) * pw));
          out[p] = Math.max(out[p], r.bins[i]);
        }
        const peak = out.reduce((m, v) => Math.max(m, v), 1);
        return out.map((v) => v / peak);
      }),
    [rows, pw],
  );

  const { drag, handlers } = useDrag(
    (x0, x1) => setView({ from: toT(Math.max(x0 - LABEL, 0), all, pw), to: toT(Math.max(x1 - LABEL, 0), all, pw) }),
    (x) => {
      const t = toT(x - LABEL, all, pw), half = (view.to - view.from) / 2;
      setView({ from: Math.max(0, t - half), to: Math.min(end, t + half) });
    },
  );

  // the rows' pixels, painted once per data and width; a view change only copies them
  const raster = useMemo(() => {
    const dpr = window.devicePixelRatio || 1;
    const c = document.createElement("canvas");
    c.width = Math.max(1, Math.round(pw * dpr));
    c.height = Math.max(1, Math.round(h * dpr));
    const ctx = c.getContext("2d");
    if (!ctx) return c;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    rows.forEach((r, ri) => {
      const y = ri * 12 + 2;
      const color = colorOf(r.label);
      px[ri].forEach((f, i) => {
        if (f <= 0) return;
        ctx.fillStyle = css(color, Math.round((0.3 + 0.7 * f) * 32) / 32);
        ctx.fillRect(i, y, 1, 9);
      });
    });
    return c;
  }, [rows, px, pw, h]);

  const draw = useCallback(
    (ctx: CanvasRenderingContext2D) => {
      ctx.font = "10px ui-monospace, monospace";
      ctx.fillStyle = css(GRAY);
      ctx.textAlign = "right";
      rows.forEach((r, ri) => ctx.fillText(r.label, LABEL - 8, ri * 12 + 10));
      ctx.textAlign = "left";
      ctx.drawImage(raster, LABEL, 0, pw, h);
      ctx.fillStyle = css([0xf2, 0xf4, 0xf7], 0.8);
      for (const t of marks) if (t >= start) ctx.fillRect(LABEL + toX(t, all, pw), h - 10, 1, 8);
      // the view: shade what is outside it
      ctx.fillStyle = "rgba(14,16,20,0.62)";
      const a = LABEL + toX(view.from, all, pw), b = LABEL + toX(view.to, all, pw);
      ctx.fillRect(LABEL, 0, a - LABEL, h);
      ctx.fillRect(b, 0, LABEL + pw - b, h);
      ctx.strokeStyle = css([0xc6, 0xcb, 0xd4], 0.7);
      ctx.strokeRect(Math.round(a) + 0.5, 0.5, Math.max(Math.round(b - a) - 1, 1), h - 1);
      if (drag) {
        ctx.fillStyle = "rgba(74,205,186,0.18)";
        ctx.fillRect(Math.min(drag.a, drag.b), 0, Math.abs(drag.b - drag.a), h);
      }
    },
    [rows, raster, marks, start, view, drag, all, pw, h],
  );
  useCanvas(canvas, w, h, draw);
  return (
    <div className="overview" ref={box}>
      <canvas ref={canvas} {...handlers} />
    </div>
  );
}

/** What the flame chart draws: zones on lanes, up to `end`, with message marks. */
export interface Chart { zones: Zone[]; lanes: string[]; end: number; marks: number[] }

interface Hover { z: Zone; x: number; y: number }

/** The flame chart of the view. Drag to zoom into a span, wheel to zoom, double-click for the run. */
export function FlameChart({ chart, view, setView, focus, color = colorOf }: { chart: Chart; view: View; setView: (v: View) => void; focus: string | null; color?: (name: string) => RGB }) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const w = useWidth(box);
  const pw = Math.max(w - LABEL, 1);
  const [hover, setHover] = useState<Hover | null>(null);

  const layout = useMemo(() => {
    const depth = new Map<string, number>();
    for (const z of chart.zones) depth.set(z.lane, Math.max(depth.get(z.lane) ?? 0, z.depth));
    let y = AXIS;
    const top = new Map<string, number>();
    for (const l of chart.lanes) {
      top.set(l, y);
      y += ((depth.get(l) ?? 0) + 1) * ROW + GAP;
    }
    // zones by lane and depth, by start, for hit tests
    const rows = new Map<string, Zone[]>();
    for (const z of chart.zones) {
      const k = `${z.lane}|${z.depth}`;
      if (!rows.has(k)) rows.set(k, []);
      rows.get(k)!.push(z);
    }
    for (const r of rows.values()) r.sort((a, b) => a.t - b.t);
    return { top, rows, h: y };
  }, [chart]);

  const at = (x: number, y: number): Zone | null => {
    for (const [lane, top] of layout.top) {
      const d = Math.floor((y - top) / ROW);
      if (d < 0) continue;
      const row = layout.rows.get(`${lane}|${d}`);
      if (!row) continue;
      const t = toT(x - LABEL, view, pw);
      let lo = 0, hi = row.length - 1, found: Zone | null = null;
      while (lo <= hi) {
        const mid = (lo + hi) >> 1;
        if (row[mid].t <= t) { found = row[mid]; lo = mid + 1; } else hi = mid - 1;
      }
      if (found && t <= found.t + found.dur) return found;
    }
    return null;
  };

  const { drag, handlers } = useDrag(
    (x0, x1) => setView({ from: toT(Math.max(x0 - LABEL, 0), view, pw), to: toT(Math.max(x1 - LABEL, 0), view, pw) }),
    (x, y) => {
      const z = at(x, y);
      if (z) {
        const pad = z.dur * 0.08;
        setView({ from: Math.max(0, z.t - pad), to: Math.min(chart.end, z.t + z.dur + pad) });
      }
    },
  );

  // the vertical wheel zooms around the pointer; a horizontal scroll (a trackpad's sideways swipe,
  // a tilt wheel, or shift with the wheel) pans along the run at the same zoom. The listener must be
  // non-passive to keep the page still.
  useEffect(() => {
    const c = canvas.current;
    if (!c) return;
    const f = (e: WheelEvent) => {
      e.preventDefault();
      const dx = e.shiftKey && e.deltaX === 0 ? e.deltaY : e.deltaX;
      if (Math.abs(dx) > Math.abs(e.deltaY) || (e.shiftKey && dx !== 0)) {
        const span = view.to - view.from;
        const shift = Math.max(-view.from, Math.min(chart.end - view.to, (dx / pw) * span));
        if (shift !== 0) setView({ from: view.from + shift, to: view.to + shift });
        return;
      }
      const r = c.getBoundingClientRect();
      const t = toT(e.clientX - r.left - LABEL, view, pw);
      const k = Math.exp(e.deltaY * 0.0015);
      const from = Math.max(0, t - (t - view.from) * k), to = Math.min(chart.end, t + (view.to - t) * k);
      if (to - from > 0.05) setView({ from, to });
    };
    c.addEventListener("wheel", f, { passive: false });
    return () => c.removeEventListener("wheel", f);
  }, [view, pw, chart.end, setView]);

  const draw = useCallback(
    (ctx: CanvasRenderingContext2D, cw: number, ch: number) => {
      ctx.font = "11px ui-monospace, monospace";
      ctx.textBaseline = "middle";
      // the axis
      ctx.fillStyle = css(GRAY);
      const ts = ticks(view.from, view.to, Math.max(2, Math.floor(pw / 110)));
      const step = ts.length > 1 ? ts[1] - ts[0] : view.to - view.from;
      for (const t of ts) {
        const x = LABEL + toX(t, view, pw);
        ctx.fillRect(Math.round(x), AXIS - 5, 1, 4);
        ctx.fillText(tickLabel(t, step, view.to), x + 3, 7);
        ctx.fillStyle = "rgba(85,93,109,0.18)";
        ctx.fillRect(Math.round(x), AXIS, 1, ch - AXIS);
        ctx.fillStyle = css(GRAY);
      }
      for (const [lane, top] of layout.top) {
        ctx.fillStyle = css([0xc6, 0xcb, 0xd4]);
        ctx.textAlign = "right";
        ctx.fillText(lane, LABEL - 8, top + ROW / 2, LABEL - 10);
        ctx.textAlign = "left";
      }
      ctx.save();
      ctx.beginPath();
      ctx.rect(LABEL, AXIS, pw, ch - AXIS);
      ctx.clip();
      // each name's two fills (bright, dimmed), formatted once a redraw
      const fills = new Map<string, [string, string]>();
      for (const z of chart.zones) {
        const x0 = LABEL + toX(z.t, view, pw), x1 = LABEL + toX(z.t + z.dur, view, pw);
        if (x1 < LABEL || x0 > cw) continue;
        const y = layout.top.get(z.lane)! + z.depth * ROW;
        const bw = Math.max(x1 - x0, 0.6);
        const dim = focus !== null && z.name !== focus;
        let f = fills.get(z.name);
        if (!f) fills.set(z.name, (f = [css(color(z.name), 0.85), css(color(z.name), 0.18)]));
        ctx.fillStyle = f[dim ? 1 : 0];
        ctx.fillRect(x0, y, bw, ROW - 1);
        if (bw > 34) {
          const label = `${z.name} ${dur(z.dur)}`;
          const x = Math.max(x0, LABEL) + 3;
          const room = Math.min(x1, cw) - x - 3;
          if (room > 20) {
            ctx.fillStyle = dim ? "rgba(242,244,247,0.35)" : "#0e1014";
            ctx.fillText(label, x, y + ROW / 2, room);
          }
        }
      }
      if (chart.marks.length) {
        ctx.strokeStyle = "rgba(242,244,247,0.35)";
        ctx.setLineDash([2, 3]);
        for (const t of chart.marks) {
          const x = LABEL + toX(t, view, pw);
          if (x < LABEL || x > cw) continue;
          ctx.beginPath();
          ctx.moveTo(Math.round(x) + 0.5, AXIS);
          ctx.lineTo(Math.round(x) + 0.5, ch);
          ctx.stroke();
        }
        ctx.setLineDash([]);
      }
      ctx.restore();
      if (drag) {
        ctx.fillStyle = "rgba(74,205,186,0.18)";
        ctx.fillRect(Math.min(drag.a, drag.b), AXIS, Math.abs(drag.b - drag.a), ch - AXIS);
      }
    },
    [chart, view, layout, pw, drag, focus, color],
  );
  useCanvas(canvas, w, layout.h, draw);

  return (
    <div className="flamechart" ref={box}>
      <canvas
        ref={canvas}
        {...handlers}
        onDoubleClick={() => setView({ from: 0, to: chart.end })}
        onMouseMove={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          const z = at(e.clientX - r.left, e.clientY - r.top);
          setHover(z ? { z, x: e.clientX - r.left, y: e.clientY - r.top } : null);
        }}
        onMouseLeave={() => setHover(null)}
      />
      {hover && (
        <div className="tip" style={{ left: Math.min(hover.x + 14, w - 260), top: hover.y + 14 }}>
          <div className="title">{hover.z.name}</div>
          <div>{dur(hover.z.dur)} on {hover.z.lane}</div>
          <div className="dim">at +{dur(hover.z.t)} · {hover.z.category}</div>
        </div>
      )}
    </div>
  );
}
