// The flame graph of the view: zones merged by their stack (lane, then the names they nest in),
// each frame as wide as its time inside the view. The server walks every zone of the range (exact
// at any zoom: GET /api/flame); click a frame to focus it, the top bar to go back.

import { useCallback, useMemo, useRef, useState } from "react";
import type { Run } from "./api";
import { useCanvas, useWidth } from "./canvas";
import { colorOf, css, dur, GRAY } from "./fmt";
import { tracked, useQuery } from "./sql";
import type { View } from "./timeline";

interface Node { name: string; total: number; count: number; children: Map<string, Node> }
interface Wire { name: string; total: number; count: number; children: Wire[] }

const ROW = 17;

const toNode = (w: Wire): Node => ({ name: w.name, total: w.total, count: w.count, children: new Map(w.children.map((c) => [c.name, toNode(c)])) });

async function loadFlame(run: Run, view: View, signal: AbortSignal): Promise<Node> {
  const qs = new URLSearchParams({ project: run.project, run: run.id, from: (run.t0 + view.from).toFixed(0), to: (run.t0 + view.to).toFixed(0) });
  return tracked(
    (async () => {
      const res = await fetch(`/api/flame?${qs}`, { signal });
      const body = await res.json();
      if (!res.ok) throw new Error(body.error ?? `${res.status}`);
      return toNode(body.root as Wire);
    })(),
  );
}

export function FlameGraph({ run, view, onPick }: { run: Run; view: View; onPick: (name: string | null) => void }) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const w = useWidth(box);
  const [focusPath, setFocusPath] = useState<string[]>([]);
  const [hover, setHover] = useState<{ n: Node; x: number; y: number; share: number } | null>(null);

  const { data } = useQuery(`flame:${run.id}:${view.from.toFixed(0)}:${view.to.toFixed(0)}`, (signal) => loadFlame(run, view, signal), 200);
  const root = data ?? { name: "all", total: 0, count: 0, children: new Map<string, Node>() };

  let focus = root;
  for (const name of focusPath) focus = focus.children.get(name) ?? focus;

  const frames = useMemo(() => {
    const out: { n: Node; x: number; w: number; d: number; path: string[] }[] = [];
    const walk = (n: Node, x: number, scale: number, d: number, path: string[]) => {
      out.push({ n, x, w: n.total * scale, d, path });
      let cx = x;
      for (const c of [...n.children.values()].sort((a, b) => a.name.localeCompare(b.name))) {
        walk(c, cx, scale, d + 1, [...path, c.name]);
        cx += c.total * scale;
      }
    };
    walk(focus, 0, focus.total > 0 ? w / focus.total : 0, 0, focusPath);
    return out;
  }, [focus, focusPath, w]);
  const depth = frames.reduce((m, f) => Math.max(m, f.d), 0);
  const h = (depth + 1) * ROW + 2;

  const draw = useCallback(
    (ctx: CanvasRenderingContext2D) => {
      ctx.font = "11px ui-monospace, monospace";
      ctx.textBaseline = "middle";
      for (const f of frames) {
        if (f.w < 0.5) continue;
        const y = f.d * ROW;
        ctx.fillStyle = f.d === 0 ? css(GRAY, 0.5) : css(colorOf(f.n.name), 0.85);
        ctx.fillRect(f.x, y, Math.max(f.w - 0.5, 0.5), ROW - 1);
        if (f.w > 40) {
          ctx.fillStyle = f.d === 0 ? "#f2f4f7" : "#0e1014";
          const label = f.d === 0 ? `${f.path.length ? f.path.join(" › ") : "all lanes"} · ${dur(f.n.total)}` : `${f.n.name} ${dur(f.n.total)}`;
          ctx.fillText(label, f.x + 3, y + ROW / 2, f.w - 6);
        }
      }
    },
    [frames],
  );
  useCanvas(canvas, w, h, draw);

  const hit = (x: number, y: number) => frames.find((f) => f.d === Math.floor(y / ROW) && x >= f.x && x < f.x + f.w);

  return (
    <div className="flamegraph" ref={box}>
      {root.total === 0 ? (
        <div className="empty">{data ? "no zones in the view" : ""}</div>
      ) : (
        <canvas
          ref={canvas}
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            const f = hit(e.clientX - r.left, e.clientY - r.top);
            if (!f) return;
            if (f.d === 0) { setFocusPath(focusPath.slice(0, -1)); onPick(null); }
            else { setFocusPath(f.path); onPick(f.path.length > 1 ? f.n.name : null); }
          }}
          onMouseMove={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            const f = hit(e.clientX - r.left, e.clientY - r.top);
            setHover(f ? { n: f.n, x: e.clientX - r.left, y: e.clientY - r.top, share: f.n.total / (focus.total || 1) } : null);
          }}
          onMouseLeave={() => setHover(null)}
        />
      )}
      {hover && (
        <div className="tip" style={{ left: Math.min(hover.x + 14, w - 260), top: hover.y + 14 }}>
          <div className="title">{hover.n.name}</div>
          <div>{dur(hover.n.total)} · {(hover.share * 100).toFixed(1)}% of {focus === root ? "all lanes" : focus.name}</div>
          {hover.n.count > 0 && (
            <div className="dim">
              {hover.n.count} zones · self {dur(hover.n.total - [...hover.n.children.values()].reduce((s, c) => s + c.total, 0))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
