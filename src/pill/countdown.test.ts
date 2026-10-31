import { describe, expect, it } from "vitest";
import {
  deadlineFrom,
  elapsedAt,
  fractionLeft,
  remainingAt,
  secondsLeft,
  smoothLevel,
  startFrom,
} from "./countdown";

describe("countdown", () => {
  it("counts down from remaining_ms using the local clock", () => {
    const deadline = deadlineFrom(3000, 10_000);
    expect(remainingAt(deadline, 10_000)).toBe(3000);
    expect(remainingAt(deadline, 11_250)).toBe(1750);
    expect(remainingAt(deadline, 13_000)).toBe(0);
    expect(remainingAt(deadline, 20_000)).toBe(0);
  });

  it("gives the fraction left for the ring", () => {
    const deadline = deadlineFrom(3000, 0);
    expect(fractionLeft(deadline, 3000, 0)).toBe(1);
    expect(fractionLeft(deadline, 3000, 1500)).toBe(0.5);
    expect(fractionLeft(deadline, 3000, 3000)).toBe(0);
    expect(fractionLeft(deadline, 0, 0)).toBe(0);
  });

  it("keeps a late event from showing a negative or oversized countdown", () => {
    const deadline = deadlineFrom(-200, 0);
    expect(remainingAt(deadline, 0)).toBe(0);
    expect(fractionLeft(deadlineFrom(4000, 0), 3000, 0)).toBe(1);
  });

  it("rounds seconds up so 3 stays until a full second passed", () => {
    const deadline = deadlineFrom(3000, 0);
    expect(secondsLeft(deadline, 0)).toBe(3);
    expect(secondsLeft(deadline, 999)).toBe(3);
    expect(secondsLeft(deadline, 1000)).toBe(2);
    expect(secondsLeft(deadline, 3000)).toBe(0);
  });
});

describe("elapsed", () => {
  it("resyncs the start from elapsed_ms and ticks locally", () => {
    const start = startFrom(12_400, 100_000);
    expect(elapsedAt(start, 100_000)).toBe(12_400);
    expect(elapsedAt(start, 100_600)).toBe(13_000);
  });

  it("never goes negative", () => {
    expect(elapsedAt(startFrom(-5, 0), 0)).toBe(0);
  });
});

describe("smoothLevel", () => {
  it("attacks at once and decays slowly", () => {
    expect(smoothLevel(0.1, 0.9)).toBe(0.9);
    const decayed = smoothLevel(0.9, 0.1);
    expect(decayed).toBeLessThan(0.9);
    expect(decayed).toBeGreaterThan(0.5);
  });

  it("clamps to 0..1", () => {
    expect(smoothLevel(0, 4)).toBe(1);
    expect(smoothLevel(0, -1)).toBe(0);
  });
});
