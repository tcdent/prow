// The flame graph of the view: zones merged by their stack (lane, then the names they nest in),
// each frame as wide as its time inside the view. Click a frame to focus it, the top bar to go back.

import { useCallback, useMemo, useRef, useState } from "react";
import type { Run } from "./api";
import { useCanvas, useWidth } from "./canvas";
import { colorOf, css, dur, GRAY } from "./fmt";
import type { View } from "./timeline";

interface Node { name: string; total: number; count: number; children: Map<string, Node> }
const node = (name: string): Node => ({ name, total: 0, count: 0, children: new Map() });

const ROW = 17;

export function FlameGraph({ run, view, onPick }: { run: Run; view: View; onPick: (name: string | null) => void }) {
  const box = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const w = useWidth(box);
  const [focusPath, setFocusPath] = useState<string[]>([]);
  const [hover, setHover] = useState<{ n: Node; x: number; y: number; share: number } | null>(null);

  const root = useMemo(() => {
    const r = node("all");
    const paths = new Map<number, string[]>();
    const path = (i: number): string[] => {
      let p = paths.get(i);
      if (p) return p;
      const z = run.zones[i];
      p = z.parent < 0 ? [z.lane, z.name] : [...path(z.parent), z.name];
      paths.set(i, p);
      return p;
    };
    run.zones.forEach((z, i) => {
      const clipped = Math.min(z.t + z.dur, view.to) - Math.max(z.t, view.from);
      if (clipped <= 0) return;
      let n = r;
      const p = path(i);
      // a lane's own total is its depth-0 zones' time, added below
      for (const name of p) {
        if (!n.children.has(name)) n.children.set(name, node(name));
        n = n.children.get(name)!;
      }
      n.total += clipped;
      n.count += 1;
      if (z.depth === 0) r.children.get(z.lane)!.total += clipped;
    });
    r.total = [...r.children.values()].reduce((s, c) => s + c.total, 0);
    return r;
  }, [run, view]);

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
        <div className="empty">no zones in the view</div>
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
