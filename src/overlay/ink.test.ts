import { describe, expect, it } from "vitest";
import {
  appendPoint,
  FADE_MS,
  HOLD_MS,
  isStroke,
  MAX_POINTS,
  strokeAlpha,
  strokeGone,
  toUnit,
} from "./ink";

describe("strokeAlpha", () => {
  it("holds, then fades to zero", () => {
    expect(strokeAlpha(0)).toBe(1);
    expect(strokeAlpha(HOLD_MS)).toBe(1);
    expect(strokeAlpha(HOLD_MS + FADE_MS / 2)).toBeCloseTo(0.5, 5);
    expect(strokeAlpha(HOLD_MS + FADE_MS)).toBe(0);
    expect(strokeAlpha(HOLD_MS + FADE_MS + 500)).toBe(0);
  });

  it("is gone only after the fade", () => {
    expect(strokeGone(HOLD_MS + FADE_MS - 1)).toBe(false);
    expect(strokeGone(HOLD_MS + FADE_MS)).toBe(true);
  });
});

describe("toUnit", () => {
  it("scales and clamps", () => {
    expect(toUnit(50, 25, 100, 50)).toEqual({ x: 0.5, y: 0.5 });
    expect(toUnit(-10, 70, 100, 50)).toEqual({ x: 0, y: 1 });
  });

  it("survives an empty window", () => {
    expect(toUnit(5, 5, 0, 0)).toEqual({ x: 0, y: 0 });
  });
});

describe("appendPoint", () => {
  it("drops points that barely moved", () => {
    const points = [{ x: 0.1, y: 0.1 }];
    expect(appendPoint(points, { x: 0.1001, y: 0.1 })).toBe(false);
    expect(appendPoint(points, { x: 0.2, y: 0.1 })).toBe(true);
    expect(points).toHaveLength(2);
  });

  it("stops at the point cap", () => {
    const points = Array.from({ length: MAX_POINTS }, (_, i) => ({ x: i / MAX_POINTS, y: 0 }));
    expect(appendPoint(points, { x: 1, y: 1 })).toBe(false);
  });
});

describe("isStroke", () => {
  it("needs two points", () => {
    expect(isStroke([{ x: 0, y: 0 }])).toBe(false);
    expect(
      isStroke([
        { x: 0, y: 0 },
        { x: 1, y: 1 },
      ]),
    ).toBe(true);
  });
});
