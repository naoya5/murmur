import { describe, expect, it } from "vitest";
import { clampOffset, hasNewer, hasOlder, pageRange } from "./historyPaging";

describe("clampOffset", () => {
  it("keeps a valid offset", () => {
    expect(clampOffset(20, 45, 20)).toBe(20);
    expect(clampOffset(0, 5, 20)).toBe(0);
  });

  it("moves to the last page when rows were deleted under the current page", () => {
    expect(clampOffset(40, 41, 20)).toBe(40);
    expect(clampOffset(40, 40, 20)).toBe(20);
    expect(clampOffset(60, 25, 20)).toBe(20);
  });

  it("goes to the first page when the history was cleared", () => {
    expect(clampOffset(40, 0, 20)).toBe(0);
  });
});

describe("pageRange", () => {
  it("is 1-based and bounded by the total", () => {
    expect(pageRange(0, 20, 45)).toEqual({ from: 1, to: 20 });
    expect(pageRange(40, 5, 45)).toEqual({ from: 41, to: 45 });
    expect(pageRange(0, 0, 0)).toEqual({ from: 0, to: 0 });
  });
});

describe("paging buttons", () => {
  it("knows when there is a newer or an older page", () => {
    expect(hasNewer(0)).toBe(false);
    expect(hasNewer(20)).toBe(true);
    expect(hasOlder(0, 45, 20)).toBe(true);
    expect(hasOlder(20, 45, 20)).toBe(true);
    expect(hasOlder(40, 45, 20)).toBe(false);
    expect(hasOlder(0, 20, 20)).toBe(false);
  });
});
