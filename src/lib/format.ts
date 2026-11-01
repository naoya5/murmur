import type { Locale, Strings } from "../i18n";

export type DurationPart = { value: string; unit: string };

/** Split a duration into at most two parts: "2 h 14 min", "48 min", "35 s". */
export function durationParts(ms: number, t: Strings): DurationPart[] {
  const totalSec = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  if (h > 0) {
    const parts = [{ value: String(h), unit: t.units.hours }];
    if (m > 0) {
      parts.push({ value: String(m), unit: t.units.minutes });
    }
    return parts;
  }
  if (m > 0) {
    const parts = [{ value: String(m), unit: t.units.minutes }];
    if (s > 0 && m < 10) {
      parts.push({ value: String(s), unit: t.units.seconds });
    }
    return parts;
  }
  if (s > 0) {
    return [{ value: String(s), unit: t.units.seconds }];
  }
  // Nothing yet: "0 min" reads as a count that will grow, "0 s" reads as broken.
  return [{ value: "0", unit: t.units.minutes }];
}

export function formatDuration(ms: number, t: Strings): string {
  const sep = t.locale === "ja" ? "" : " ";
  return durationParts(ms, t)
    .map((p) => `${p.value}${sep}${p.unit}`)
    .join(" ");
}

/** "mm:ss" for the pill timer. Minutes grow past 59 without wrapping. */
export function formatClock(ms: number): string {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
  return `${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
}

export function formatNumber(n: number, locale: Locale, maxFractionDigits = 0): string {
  return new Intl.NumberFormat(locale, { maximumFractionDigits: maxFractionDigits }).format(n);
}

/** Decimal units, as macOS shows file sizes. */
export function formatBytes(bytes: number, locale: Locale): string {
  if (bytes >= 1e9) {
    return `${formatNumber(bytes / 1e9, locale, 2)} GB`;
  }
  if (bytes >= 1e6) {
    return `${formatNumber(bytes / 1e6, locale, 0)} MB`;
  }
  if (bytes >= 1e3) {
    return `${formatNumber(bytes / 1e3, locale, 0)} KB`;
  }
  return `${formatNumber(bytes, locale)} B`;
}

/** "YYYY-MM-DD" to a local Date (no UTC shift). */
export function parseDay(day: string): Date {
  const [y, m, d] = day.split("-").map(Number);
  return new Date(y, (m || 1) - 1, d || 1);
}

export function toDayKey(date: Date): string {
  const y = date.getFullYear();
  const m = String(date.getMonth() + 1).padStart(2, "0");
  const d = String(date.getDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

function sameDay(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

/** Time only for today, date and time otherwise; the year only when it differs. */
export function formatWhen(ms: number, locale: Locale, now = new Date()): string {
  const date = new Date(ms);
  const time = new Intl.DateTimeFormat(locale, { hour: "2-digit", minute: "2-digit" }).format(date);
  if (sameDay(date, now)) {
    return time;
  }
  const day = new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    ...(date.getFullYear() === now.getFullYear() ? {} : { year: "numeric" }),
  }).format(date);
  return `${day} ${time}`;
}

export function formatShortDay(date: Date, locale: Locale): string {
  return new Intl.DateTimeFormat(locale, { month: "numeric", day: "numeric" }).format(date);
}

export function formatMonth(date: Date, locale: Locale): string {
  return new Intl.DateTimeFormat(locale, { month: "short" }).format(date);
}

export function formatPercent(fraction: number, locale: Locale): string {
  return new Intl.NumberFormat(locale, { style: "percent", maximumFractionDigits: 0 }).format(
    Math.min(1, Math.max(0, fraction)),
  );
}
