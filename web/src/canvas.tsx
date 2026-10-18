// Canvas plumbing shared by every chart: the element's width, device-pixel drawing, and a drag
// that selects a span of time (the view's range) or, without moving, clicks.

import { useEffect, useLayoutEffect, useRef, useState, type RefObject } from "react";

export function useWidth(ref: RefObject<HTMLElement | null>): number {
  const [w, setW] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setW(Math.floor(e.contentRect.width)));
    ro.observe(el);
    setW(Math.floor(el.getBoundingClientRect().width));
    return () => ro.disconnect();
  }, [ref]);
  return w;
}

export type Draw = (ctx: CanvasRenderingContext2D, w: number, h: number) => void;

/** Draw on a canvas at device resolution whenever `draw` changes. */
export function useCanvas(ref: RefObject<HTMLCanvasElement | null>, w: number, h: number, draw: Draw) {
  useEffect(() => {
    const c = ref.current;
    if (!c || w <= 0) return;
    const dpr = window.devicePixelRatio || 1;
    c.width = Math.round(w * dpr);
    c.height = Math.round(h * dpr);
    c.style.width = `${w}px`;
    c.style.height = `${h}px`;
    const ctx = c.getContext("2d");
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    draw(ctx, w, h);
  }, [ref, w, h, draw]);
}

export interface Drag { a: number; b: number }

/**
 * Pointer handling over a time axis: a drag reports the dragged x span (CSS px, ordered) on release
 * through `onSpan`; a press and release in place reports `onClick(x, y)`. `drag` is the span while
 * the pointer is down, for an overlay.
 */
export function useDrag(onSpan: (x0: number, x1: number) => void, onClick?: (x: number, y: number) => void) {
  const [drag, setDrag] = useState<Drag | null>(null);
  const start = useRef<{ x: number; y: number } | null>(null);
  const handlers = {
    onPointerDown(e: React.PointerEvent) {
      if (e.button !== 0) return;
      (e.target as Element).setPointerCapture(e.pointerId);
      const r = (e.currentTarget as Element).getBoundingClientRect();
      start.current = { x: e.clientX - r.left, y: e.clientY - r.top };
      setDrag({ a: start.current.x, b: start.current.x });
    },
    onPointerMove(e: React.PointerEvent) {
      if (!start.current) return;
      const r = (e.currentTarget as Element).getBoundingClientRect();
      setDrag({ a: start.current.x, b: e.clientX - r.left });
    },
    onPointerUp(e: React.PointerEvent) {
      const s = start.current;
      start.current = null;
      setDrag(null);
      if (!s) return;
      const r = (e.currentTarget as Element).getBoundingClientRect();
      const x = e.clientX - r.left;
      if (Math.abs(x - s.x) < 4) onClick?.(s.x, s.y);
      else onSpan(Math.min(s.x, x), Math.max(s.x, x));
    },
  };
  return { drag, handlers };
}
