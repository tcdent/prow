// A site's duration fields as layers: the menu that switches them, and the view's totals by layer
// (split by a label when one is picked). Every layer is on until switched off; layers that overlap
// (a phase and its breakdown) are the reader's to choose. The rows over time are the flame chart's.

import { useRef } from "react";
import { useWidth } from "./canvas";
import { css, dur, type RGB } from "./fmt";


export function Layers({ layers, colors, on, setOn }: { layers: string[]; colors: RGB[]; on: boolean[]; setOn: (on: boolean[]) => void }) {
  const count = on.filter(Boolean).length;
  return (
    <details className="layers">
      <summary>layers {count}/{layers.length}</summary>
      <div className="layer-menu">
        <div className="layer-actions">
          <button onClick={() => setOn(layers.map(() => true))}>all</button>
          <button onClick={() => setOn(layers.map(() => false))}>none</button>
        </div>
        {layers.map((l, i) => (
          <label key={l}>
            <input type="checkbox" checked={on[i]} onChange={() => setOn(on.map((v, j) => (j === i ? !v : v)))} />
            <i style={{ background: css(colors[i]) }} />
            {l}
          </label>
        ))}
      </div>
    </details>
  );
}

/** A group's mean a row of each layer (µs), and its row count. */
export interface GroupMeans { g: string; n: number; mean: number[] }
/** A layer's figures over the view (µs). */
export interface LayerStats { i: number; total: number; mean: number; p50: number; p90: number; max: number }

/** The view's time by layer: one stacked bar a group (its mean a row), and a table of every layer. */
export function StackTotals({ layers, colors, on, setOn, groups, stats }: { layers: string[]; colors: RGB[]; on: boolean[]; setOn: (on: boolean[]) => void; groups: GroupMeans[]; stats: LayerStats[] }) {
  const box = useRef<HTMLDivElement>(null);
  const w = useWidth(box);
  const longest = Math.max(1e-9, ...groups.map((g) => g.mean.reduce((s, v, i) => s + (on[i] ? v : 0), 0)));
  const enabled = stats.reduce((s, x) => s + (on[x.i] ? x.total : 0), 0);
  const barW = Math.max(w - 270, 1);

  return (
    <div ref={box}>
      {groups.map((g) => {
        let x = 0;
        return (
          <div className="stackbar" key={g.g}>
            <span className="group">{g.g || "all"} <span className="dim">×{g.n}</span></span>
            <div className="bar" style={{ width: barW }}>
              {g.mean.map((v, i) => {
                if (!on[i] || !(v > 0)) return null;
                const width = (v / longest) * barW;
                const left = x;
                x += width;
                return (
                  <span key={i} className="seg" title={`${layers[i]} ${dur(v)} a row`} style={{ left, width, background: css(colors[i], 0.85) }}>
                    {width > 60 ? `${layers[i]} ${dur(v)}` : ""}
                  </span>
                );
              })}
            </div>
            <span className="dim">{dur(g.mean.reduce((s, v, i) => s + (on[i] ? v : 0), 0))} a row</span>
          </div>
        );
      })}
      <table className="stats">
        <thead>
          <tr><th>layer</th><th></th><th>in view</th><th>share</th><th>mean</th><th>p50</th><th>p90</th><th>max</th></tr>
        </thead>
        <tbody>
          {[...stats].sort((a, b) => b.total - a.total).map((s) => (
            <tr key={s.i} className={on[s.i] ? "" : "off"} onClick={() => setOn(on.map((v, j) => (j === s.i ? !v : v)))} title="click to switch the layer on or off">
              <td><i className="sw" style={{ background: css(colors[s.i]) }} />{layers[s.i]}</td>
              <td className="dim">{on[s.i] ? "" : "off"}</td>
              <td className="num">{dur(s.total)}</td>
              <td className="num">{on[s.i] && enabled > 0 ? `${((s.total / enabled) * 100).toFixed(1)}%` : "—"}</td>
              <td className="num">{dur(s.mean)}</td>
              <td className="num">{dur(s.p50)}</td>
              <td className="num">{dur(s.p90)}</td>
              <td className="num">{dur(s.max)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
