// Durations, numbers and colours. Times are µs from the run's start throughout.

export type RGB = [number, number, number];
// the subject colours
export const PINK: RGB = [0xac, 0x4c, 0xa4];
export const BLUE: RGB = [0x4f, 0x60, 0xe1];
export const TEAL: RGB = [0x4a, 0xcd, 0xba];
export const GREEN: RGB = [0x6d, 0xcd, 0x66];
export const OLIVE: RGB = [0xa9, 0xa5, 0x68];
export const RED: RGB = [0xe0, 0x52, 0x4f];
export const GRAY: RGB = [0x55, 0x5d, 0x6d];
const PALETTE = [TEAL, BLUE, PINK, GREEN, OLIVE, [0x7f, 0x8c, 0xe8], [0x5f, 0xa8, 0xd8], [0xc9, 0x7b, 0x5a]] as RGB[];

export function css([r, g, b]: RGB, a = 1): string {
  return a === 1 ? `rgb(${r},${g},${b})` : `rgba(${r},${g},${b},${a})`;
}

function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return h >>> 0;
}

/** A stable colour for a name: the same zone is the same colour in every view and run. */
export function colorOf(name: string): RGB {
  const base = PALETTE[hash(name) % PALETTE.length];
  const k = 0.78 + ((hash(name + "~") % 100) / 100) * 0.3;
  return base.map((c) => Math.min(255, Math.round(c * k))) as RGB;
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

/** Local wall-clock time of µs `t` into a run that began at `t0` (ns since the epoch). */
export function wall(t0: bigint, t: number): string {
  const ms = Number(t0 / 1_000_000n) + t / 1e3;
  return new Date(ms).toISOString().replace("T", " ").replace("Z", " UTC");
}
