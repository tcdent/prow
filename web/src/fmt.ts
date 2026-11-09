// Durations, numbers and colors. Times are µs from the run's start throughout.

export type RGB = [number, number, number];
// the subject colors
export const PINK: RGB = [0xac, 0x4c, 0xa4];
export const BLUE: RGB = [0x4f, 0x60, 0xe1];
export const TEAL: RGB = [0x4a, 0xcd, 0xba];
export const GREEN: RGB = [0x6d, 0xcd, 0x66];
export const OLIVE: RGB = [0xa9, 0xa5, 0x68];
export const RED: RGB = [0xe0, 0x52, 0x4f];
export const GRAY: RGB = [0x80, 0x89, 0x9a];
const PALETTE = [TEAL, BLUE, PINK, GREEN, OLIVE, [0x7f, 0x8c, 0xe8], [0x5f, 0xa8, 0xd8], [0xc9, 0x7b, 0x5a]] as RGB[];

// formatted colors, kept: a chart redraw asks for the same few thousands of times
const cssCache = new Map<string, string>();

export function css([r, g, b]: RGB, a = 1): string {
  const key = `${r},${g},${b},${a}`;
  let s = cssCache.get(key);
  if (s === undefined) {
    if (cssCache.size > 4096) cssCache.clear();
    s = a === 1 ? `rgb(${r},${g},${b})` : `rgba(${r},${g},${b},${a})`;
    cssCache.set(key, s);
  }
  return s;
}

function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return h >>> 0;
}

/** A stable color for a name: the same zone is the same color in every view and run. */
export function colorOf(name: string): RGB {
  const base = PALETTE[hash(name) % PALETTE.length];
  const k = 0.78 + ((hash(name + "~") % 100) / 100) * 0.3;
  return base.map((c) => Math.min(255, Math.round(c * k))) as RGB;
}

/** Tableau's 20 colors: made to be told apart, in the order they are most distinct. */
const TAB20: RGB[] = ["#4e79a7", "#f28e2b", "#59a14f", "#e15759", "#b07aa1", "#edc948", "#76b7b2", "#ff9da7", "#9c755f", "#bab0ac",
  "#a0cbe8", "#ffbe7d", "#8cd17d", "#f1ce63", "#86bcb6", "#d37295", "#fabfd2", "#b6992d", "#499894", "#d4a6c8"].map(
  (h) => [parseInt(h.slice(1, 3), 16), parseInt(h.slice(3, 5), 16), parseInt(h.slice(5, 7), 16)] as RGB,
);

/** Colors for a stack's layers, given each layer's total: the heaviest get the most distinct. */
export function layerColors(totals: number[]): RGB[] {
  const rank = totals.map((_, i) => i).sort((a, b) => totals[b] - totals[a]);
  const out: RGB[] = new Array(totals.length);
  rank.forEach((i, r) => (out[i] = TAB20[r % TAB20.length]));
  return out;
}

/** A field named like a duration reads as one: `s`, `ms`, `us`, `ns`, or ending in `_s`, `_ms`, `_us`, `_ns`. */
export function isDuration(name: string): boolean {
  return /(^|_)(s|ms|us|ns)$/.test(name);
}

/** Microseconds in one unit of a duration field. */
export function durationScale(name: string): number {
  return /(^|_)s$/.test(name) ? 1e6 : /(^|_)ms$/.test(name) ? 1e3 : /(^|_)ns$/.test(name) ? 1e-3 : 1;
}

/** `850 ns`, `12.3 µs`, `4.56 ms`, `1.20 s`, `3m 05s`. */
export function dur(us: number): string {
  const a = Math.abs(us);
  if (!Number.isFinite(us)) return "—";
  if (us === 0) return "0";
  if (a < 1) return `${Math.round(us * 1000)} ns`;
  if (a < 1000) return `${sig(us)} µs`;
  if (a < 1e6) return `${sig(us / 1e3)} ms`;
  if (a < 6e7) return `${sig(us / 1e6)} s`;
  const s = Math.round(us / 1e6);
  return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
}

/** Three significant digits. */
export function sig(v: number): string {
  const a = Math.abs(v);
  if (a === 0) return "0";
  if (a >= 100) return v.toFixed(0);
  if (a >= 10) return v.toFixed(1);
  return v.toFixed(2);
}

export function num(v: number): string {
  if (!Number.isFinite(v)) return "—";
  const a = Math.abs(v);
  if (a >= 1e9) return `${sig(v / 1e9)}G`;
  if (a >= 1e6) return `${sig(v / 1e6)}M`;
  if (a >= 1e4) return `${sig(v / 1e3)}k`;
  if (Number.isInteger(v)) return String(v);
  return sig(v);
}

/** Tick positions for a span: a nice step giving about `n` ticks. */
export function ticks(from: number, to: number, n: number): number[] {
  const span = Math.max(to - from, 1e-9);
  const raw = span / n;
  const p = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 5, 10].map((m) => m * p).find((s) => s >= raw) ?? 10 * p;
  const out: number[] = [];
  for (let t = Math.ceil(from / step) * step; t <= to; t += step) out.push(t);
  return out;
}

/** A tick's label: in the unit of the axis's largest time `max`, with the decimals the step needs. */
export function tickLabel(t: number, step: number, max: number): string {
  const [div, unit] = max >= 1e6 ? [1e6, "s"] : max >= 1e3 ? [1e3, "ms"] : [1, "µs"];
  const decimals = Math.min(6, Math.max(0, Math.ceil(-Math.log10(step / div) - 1e-9)));
  return `${(t / div).toFixed(decimals)} ${unit}`;
}

export function quantile(sorted: number[], q: number): number {
  if (!sorted.length) return NaN;
  const i = (sorted.length - 1) * q;
  const lo = Math.floor(i), hi = Math.ceil(i);
  return sorted[lo] + (sorted[hi] - sorted[lo]) * (i - lo);
}

/** UTC wall-clock time of µs `t` into a run whose t = 0 is `t0` (µs since the epoch). */
export function wall(t0: number, t: number): string {
  return new Date((t0 + t) / 1000).toISOString().replace("T", " ").replace("Z", " UTC");
}
