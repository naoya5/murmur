import { describe, expect, it } from "vitest";
import { strings } from "../i18n";
import {
  durationParts,
  formatBytes,
  formatClock,
  formatDuration,
  parseDay,
  toDayKey,
} from "./format";

const en = strings.en;
const ja = strings.ja;

describe("durations", () => {
  it("splits into at most two parts", () => {
    expect(durationParts(2 * 3600_000 + 14 * 60_000 + 9_000, en)).toEqual([
      { value: "2", unit: "h" },
      { value: "14", unit: "min" },
    ]);
    expect(durationParts(48 * 60_000, en)).toEqual([{ value: "48", unit: "min" }]);
    expect(durationParts(3 * 60_000 + 5_000, en)).toEqual([
      { value: "3", unit: "min" },
      { value: "5", unit: "s" },
    ]);
    expect(durationParts(35_000, en)).toEqual([{ value: "35", unit: "s" }]);
    expect(durationParts(0, en)).toEqual([{ value: "0", unit: "min" }]);
  });

  it("joins without a space in Japanese", () => {
    expect(formatDuration(2 * 3600_000 + 14 * 60_000, ja)).toBe("2時間 14分");
    expect(formatDuration(2 * 3600_000 + 14 * 60_000, en)).toBe("2 h 14 min");
  });

  it("formats the pill clock with tabular minutes", () => {
    expect(formatClock(0)).toBe("00:00");
    expect(formatClock(12_400)).toBe("00:12");
    expect(formatClock(61 * 60_000)).toBe("61:00");
  });
});

describe("bytes", () => {
  it("uses decimal units like Finder", () => {
    expect(formatBytes(656_381_472, "en")).toBe("656 MB");
    expect(formatBytes(1_238_624, "en")).toBe("1 MB");
    expect(formatBytes(1_500_000_000, "en")).toBe("1.5 GB");
  });
});

describe("days", () => {
  it("parses YYYY-MM-DD as a local date", () => {
    const d = parseDay("2026-11-01");
    expect(d.getFullYear()).toBe(2026);
    expect(d.getMonth()).toBe(10);
    expect(d.getDate()).toBe(1);
    expect(toDayKey(d)).toBe("2026-11-01");
  });
});
