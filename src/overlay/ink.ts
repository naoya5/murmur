// Pure helpers of the drawing layer: timing of the fade and the unit-square mapping.
// Kept free of the DOM so they can be tested without a canvas.

import type { InkPoint } from "../ipc";

/** A stroke stays fully visible this long after the pointer is released. */
export const HOLD_MS = 650;
/** Then it fades out over this long. */
export const FADE_MS = 850;
/** Points closer than this (as a share of the screen) to the last kept point are dropped. */
export const MIN_STEP = 0.0015;
/** A stroke longer than this many points is cut: the backend does not need more detail. */
export const MAX_POINTS = 2000;

/** Opacity of a finished stroke `elapsedMs` after its end: 1 while held, then a linear fade. */
export function strokeAlpha(elapsedMs: number): number {
  if (elapsedMs <= HOLD_MS) {
    return 1;
  }
  return Math.max(0, 1 - (elapsedMs - HOLD_MS) / FADE_MS);
}

/** True once a finished stroke is fully gone and can be dropped. */
export function strokeGone(elapsedMs: number): boolean {
  return elapsedMs >= HOLD_MS + FADE_MS;
}

/** Maps a pixel position to the 0..1 square of the window, clamped. */
export function toUnit(x: number, y: number, width: number, height: number): InkPoint {
  if (width <= 0 || height <= 0) {
    return { x: 0, y: 0 };
  }
  return {
    x: Math.min(1, Math.max(0, x / width)),
    y: Math.min(1, Math.max(0, y / height)),
  };
}

/** Appends `next` unless it is too close to the last point or the stroke is full. */
export function appendPoint(points: InkPoint[], next: InkPoint): boolean {
  if (points.length >= MAX_POINTS) {
    return false;
  }
  const last = points[points.length - 1];
  if (last && Math.hypot(next.x - last.x, next.y - last.y) < MIN_STEP) {
    return false;
  }
  points.push(next);
  return true;
}

/** A stroke needs a visible length; a plain click is not a line. */
export function isStroke(points: InkPoint[]): boolean {
  return points.length >= 2;
}
