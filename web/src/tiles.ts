// Time tiles: a fixed grid of slices of the run, each width a power of two (µs), each fetched once
// and cached. The view picks the level that puts two to four tiles across it; a level's tiles keep
// only items at least width / 1024 long (about a pixel), so zooming in brings finer items. While
// a level's tiles load, cached coarser tiles stand in.

import { useEffect, useMemo, useRef, useState } from "react";
import type { View } from "./timeline";

export interface Tile<T> { items: T[]; truncated: boolean }
export type LoadTile<T> = (start: number, width: number, cutoff: number, signal: AbortSignal) => Promise<Tile<T>>;

const MAX_TILES = 600;
const IN_FLIGHT = 6;

/** The level for a view: its tiles' width (µs). */
export function tileWidth(span: number): number {
  return 2 ** Math.max(0, Math.ceil(Math.log2(Math.max(span / 2, 1))));
}

/**
 * The items of `source` in the view, from its tiles. `source` names what is tiled (and its
 * settings that change a tile's content); a new `source` starts a new cache.
 */
export function useTiles<T>(source: string, view: View, end: number, load: LoadTile<T>, idOf: (x: T) => string, enabled = true): { items: T[]; loading: boolean; truncated: boolean } {
  const cache = useRef(new Map<string, Tile<T>>());
  const pending = useRef(new Set<string>());
  const owner = useRef(source);
  const loadRef = useRef(load);
  loadRef.current = load;
  const [version, setVersion] = useState(0);
  if (owner.current !== source) {
    owner.current = source;
    cache.current = new Map();
    pending.current = new Set();
  }

  const w = tileWidth(view.to - view.from);
  const last = Math.ceil(end / w);
  const first = Math.max(0, Math.floor(view.from / w));
  const final = Math.min(last, Math.floor(view.to / w));
  const key = (width: number, k: number) => `${width}:${k}`;

  // fetch what the view needs, and a tile either side, once the view has settled
  const wanted = useMemo(() => {
    const ks: number[] = [];
    for (let k = first; k <= final; k++) ks.push(k);
    if (first > 0) ks.push(first - 1);
    if (final < last) ks.push(final + 1);
    return ks;
  }, [first, final, last]);
  useEffect(() => {
    if (!enabled) return;
    const src = source;
    const timer = setTimeout(() => {
      for (const k of wanted) {
        const id = key(w, k);
        if (cache.current.has(id) || pending.current.has(id) || pending.current.size >= IN_FLIGHT) continue;
        pending.current.add(id);
        const ctl = new AbortController();
        loadRef.current(k * w, w, w / 1024, ctl.signal).then(
          (tile) => {
            if (owner.current !== src) return;
            pending.current.delete(id);
            cache.current.set(id, tile);
            while (cache.current.size > MAX_TILES) cache.current.delete(cache.current.keys().next().value!);
            setVersion((v) => v + 1);
          },
          () => {
            if (owner.current === src) pending.current.delete(id);
          },
        );
      }
    }, 100);
    return () => clearTimeout(timer);
  }, [source, w, wanted, version, enabled]); // eslint-disable-line react-hooks/exhaustive-deps

  return useMemo(() => {
    const seen = new Set<string>();
    const items: T[] = [];
    let missing = false, truncated = false;
    const add = (tile: Tile<T>) => {
      truncated ||= tile.truncated;
      for (const x of tile.items) {
        const id = idOf(x);
        if (!seen.has(id)) {
          seen.add(id);
          items.push(x);
        }
      }
    };
    for (let k = first; k <= final; k++) {
      const tile = cache.current.get(key(w, k));
      if (tile) {
        add(tile);
        continue;
      }
      missing = true;
      // a coarser tile covering this one, if one is cached
      for (let cw = w * 2; cw <= 2 ** 42; cw *= 2) {
        const coarse = cache.current.get(key(cw, Math.floor((k * w) / cw)));
        if (coarse) {
          add(coarse);
          break;
        }
      }
    }
    return { items, loading: missing, truncated };
  }, [source, w, first, final, version]); // eslint-disable-line react-hooks/exhaustive-deps
}
