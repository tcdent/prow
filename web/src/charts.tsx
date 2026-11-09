// Charts over the view: a value over time (lines or dots, colored by a label) and a histogram.
// A drag on a time chart selects that span as the view, as on the flame chart.

import { useCallback, useMemo, useRef, useState } from "react";
import { useCanvas, useDrag, useWidth } from "./canvas";
import { colorOf, css, dur, num, tickLabel, ticks, GRAY } from "./fmt";
import type { View } from "./timeline";

export interface Point { t: number; v: number; g: string }

const LEFT = 52, BOTTOM = 16, H = 150;

function extent(vs: number[]): [number, number] {
  let lo = Infinity, hi = -Infinity;
  for (const v of vs) if (Number.isFinite(v)) { lo = Math.min(lo, v); hi = Math.max(hi, v); }
  if (!Number.isFinite(lo)) return [0, 1];
  if (lo === hi) return [Math.min(0, lo), hi === 0 ? 1 : Math.max(0, hi) * 1.05 || 1];
  const pad = (hi - lo) * 0.06;
  return [lo >= 0 && lo - pad < 0 ? 0 : lo - pad, hi + pad];
}

function Legend({ groups }: { groups: string[] }) {
  if (groups.length < 2) return null;
  return (
    <div className="legend">
      {groups.map((g) => (
        <span key={g}><i style={{ background: css(colorOf(g)) }} />{g}</span>
      ))}
    </div>
  );
}

export function TimeChart({ points, view, setView, mode, unit }: { points: Point[]; view: View; setView: (v: View) => void; mode: "line" | "dots"; unit?: (v: number) => string }) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const w = useWidth(box);
  const pw = Math.max(w - LEFT, 1), ph = H - BOTTOM;
  const fmtV = unit ?? num;
  const inView = useMemo(() => points.filter((p) => p.t >= view.from && p.t <= view.to), [points, view]);
  const [lo, hi] = useMemo(() => extent(inView.map((p) => p.v)), [inView]);
  const groups = useMemo(() => [...new Set(points.map((p) => p.g))].sort(), [points]);
  const [hover, setHover] = useState<{ p: Point; x: number; y: number } | null>(null);
  const X = (t: number) => LEFT + ((t - view.from) / (view.to - view.from)) * pw;
  const Y = (v: number) => ph - ((v - lo) / (hi - lo || 1)) * (ph - 4);

  const { drag, handlers } = useDrag((x0, x1) =>
    setView({ from: view.from + ((Math.max(x0, LEFT) - LEFT) / pw) * (view.to - view.from), to: view.from + ((Math.max(x1, LEFT) - LEFT) / pw) * (view.to - view.from) }),
  );

  const draw = useCallback(
    (ctx: CanvasRenderingContext2D) => {
      ctx.font = "11px ui-monospace, monospace";
      ctx.textBaseline = "middle";
      ctx.fillStyle = css(GRAY);
      for (const v of ticks(lo, hi, 3)) {
        const y = Y(v);
        ctx.fillText(fmtV(v), 0, y);
        ctx.fillStyle = "rgba(85,93,109,0.22)";
        ctx.fillRect(LEFT, Math.round(y), pw, 1);
        ctx.fillStyle = css(GRAY);
      }
      const ts = ticks(view.from, view.to, Math.max(2, Math.floor(pw / 120)));
      for (const t of ts) ctx.fillText(tickLabel(t, ts.length > 1 ? ts[1] - ts[0] : view.to - view.from, view.to), X(t) + 2, H - 6);
      ctx.save();
      ctx.beginPath();
      ctx.rect(LEFT, 0, pw, ph);
      ctx.clip();
      for (const g of groups) {
        const pts = inView.filter((p) => p.g === g);
        ctx.strokeStyle = ctx.fillStyle = css(colorOf(g), 0.9);
        if (mode === "line") {
          ctx.beginPath();
          pts.forEach((p, i) => (i ? ctx.lineTo(X(p.t), Y(p.v)) : ctx.moveTo(X(p.t), Y(p.v))));
          ctx.stroke();
          if (pts.length < 60) for (const p of pts) ctx.fillRect(X(p.t) - 1.5, Y(p.v) - 1.5, 3, 3);
        } else for (const p of pts) ctx.fillRect(X(p.t) - 1, Y(p.v) - 1, 2.5, 2.5);
      }
      ctx.restore();
      if (drag) {
        ctx.fillStyle = "rgba(74,205,186,0.18)";
        ctx.fillRect(Math.min(drag.a, drag.b), 0, Math.abs(drag.b - drag.a), ph);
      }
    },
    [inView, groups, lo, hi, view, pw, mode, drag],
  );
  useCanvas(canvas, w, H, draw);

  return (
    <div className="chart-box" ref={box}>
      <Legend groups={groups} />
      <canvas
        ref={canvas}
        {...handlers}
        onMouseMove={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          const x = e.clientX - r.left, y = e.clientY - r.top;
          let best: Point | null = null, bd = 64;
          for (const p of inView) {
            const d = (X(p.t) - x) ** 2 + (Y(p.v) - y) ** 2;
            if (d < bd) { bd = d; best = p; }
          }
          setHover(best ? { p: best, x, y } : null);
        }}
        onMouseLeave={() => setHover(null)}
      />
      {hover && (
        <div className="tip" style={{ left: Math.min(hover.x + 14, w - 220), top: hover.y + 14 }}>
          <div className="title">{fmtV(hover.p.v)}</div>
          <div className="dim">{hover.p.g && `${hover.p.g} · `}at +{dur(hover.p.t)}</div>
        </div>
      )}
    </div>
  );
}

/** Bins counted by the server: `counts[group][bin]` over [lo, hi]. */
export interface Bins { lo: number; hi: number; groups: string[]; counts: number[][] }
export const BINS = 48;

export function Histogram({ data, unit }: { data: Bins; unit?: (v: number) => string }) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const w = useWidth(box);
  const pw = Math.max(w - LEFT, 1), ph = H - BOTTOM;
  const fmtV = unit ?? num;
  const { lo, groups, counts } = data;
  const hi = data.hi > data.lo ? data.hi : data.lo + 1;
  const peak = useMemo(() => {
    let p = 1;
    for (let b = 0; b < BINS; b++) p = Math.max(p, counts.reduce((s, c) => s + (c[b] ?? 0), 0));
    return p;
  }, [counts]);
  const [hover, setHover] = useState<{ b: number; x: number; y: number } | null>(null);

  const draw = useCallback(
    (ctx: CanvasRenderingContext2D) => {
      ctx.font = "11px ui-monospace, monospace";
      ctx.textBaseline = "middle";
      ctx.fillStyle = css(GRAY);
      ctx.fillText(String(peak), 0, 6);
      ctx.fillText("0", 0, ph);
      for (const v of ticks(lo, hi, Math.max(2, Math.floor(pw / 110)))) ctx.fillText(fmtV(v), LEFT + ((v - lo) / (hi - lo || 1)) * pw, H - 6);
      const bw = pw / BINS;
      for (let b = 0; b < BINS; b++) {
        let y = ph;
        groups.forEach((g, gi) => {
          const hgt = (counts[gi][b] / peak) * (ph - 4);
          if (!hgt) return;
          ctx.fillStyle = css(colorOf(g), 0.85);
          ctx.fillRect(LEFT + b * bw, y - hgt, Math.max(bw - 1, 1), hgt);
          y -= hgt;
        });
      }
    },
    [counts, groups, peak, lo, hi, pw],
  );
  useCanvas(canvas, w, H, draw);
  const bw = (hi - lo) / BINS;
  return (
    <div className="chart-box" ref={box}>
      <Legend groups={groups} />
      <canvas
        ref={canvas}
        onMouseMove={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          const b = Math.floor(((e.clientX - r.left - LEFT) / pw) * BINS);
          setHover(b >= 0 && b < BINS ? { b, x: e.clientX - r.left, y: e.clientY - r.top } : null);
        }}
        onMouseLeave={() => setHover(null)}
      />
      {hover && (
        <div className="tip" style={{ left: Math.min(hover.x + 14, w - 220), top: hover.y + 14 }}>
          <div className="title">{fmtV(lo + hover.b * bw)} – {fmtV(lo + (hover.b + 1) * bw)}</div>
          {groups.map((g, gi) => counts[gi][hover.b] > 0 && <div key={g}>{g ? `${g}: ` : ""}{counts[gi][hover.b]}</div>)}
        </div>
      )}
    </div>
  );
}
