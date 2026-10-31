// Hand-made SVG charts: heatmap (20 weeks), bars (14 days), sparkline. No chart library.
import { useLayoutEffect, useRef, useState } from "react";
import type { Locale } from "../i18n";
import type { DayPoint } from "../ipc";
import { formatMonth, formatShortDay, parseDay } from "../lib/format";

type HeatmapProps = {
  days: DayPoint[];
  locale: Locale;
  label: string;
};

/** 140 days as 20 columns of 7 rows, Sunday on top, oldest column left. */
export function Heatmap({ days, locale, label }: HeatmapProps) {
  const cell = 11;
  const gap = 3;
  const step = cell + gap;
  const labelH = 14;
  const first = days[0] ? parseDay(days[0].date) : new Date();
  const offset = first.getDay();
  const columns = Math.ceil((days.length + offset) / 7);
  const width = columns * step - gap;
  const height = 7 * step - gap + labelH;
  const max = days.reduce((m, d) => Math.max(m, d.sessions), 0);
  const level = (sessions: number): number => {
    if (sessions <= 0 || max <= 0) {
      return 0;
    }
    return Math.min(4, Math.max(1, Math.ceil((sessions / max) * 4)));
  };

  const months: { x: number; text: string }[] = [];
  let lastMonth = -1;
  days.forEach((d, i) => {
    const date = parseDay(d.date);
    const col = Math.floor((i + offset) / 7);
    if (date.getMonth() !== lastMonth && date.getDate() <= 7) {
      lastMonth = date.getMonth();
      if (months.length === 0 || col - (months[months.length - 1]?.x ?? 0) / step > 2) {
        months.push({ x: col * step, text: formatMonth(date, locale) });
      }
    }
  });

  return (
    <svg
      className="chart"
      viewBox={`0 0 ${width} ${height}`}
      role="img"
      aria-label={label}
      preserveAspectRatio="xMidYMid meet"
    >
      <title>{label}</title>
      {months.map((m) => (
        <text key={m.x} x={m.x} y={9} className="heatmap__month">
          {m.text}
        </text>
      ))}
      {days.map((d, i) => {
        const col = Math.floor((i + offset) / 7);
        const row = (i + offset) % 7;
        return (
          <rect
            key={d.date}
            className="heatmap__cell"
            data-level={level(d.sessions)}
            x={col * step}
            y={labelH + row * step}
            width={cell}
            height={cell}
            rx={3}
          >
            <title>{`${formatShortDay(parseDay(d.date), locale)}: ${d.sessions}`}</title>
          </rect>
        );
      })}
    </svg>
  );
}

type BarsProps = {
  days: DayPoint[];
  locale: Locale;
  label: string;
  /** What each bar measures; sessions by default. */
  measure?: (d: DayPoint) => number;
};

/** The rendered width of an element, kept current by a ResizeObserver. */
function useMeasuredWidth<T extends HTMLElement>(
  fallback: number,
): [React.RefObject<T | null>, number] {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(fallback);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) {
      return;
    }
    const update = () => setWidth(Math.max(1, Math.round(el.getBoundingClientRect().width)));
    update();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, width];
}

export function Bars({ days, locale, label, measure = (d) => d.sessions }: BarsProps) {
  const [hostRef, width] = useMeasuredWidth<HTMLDivElement>(420);
  const chartH = 72;
  const labelH = 14;
  const gap = 6;
  const n = Math.max(1, days.length);
  const barW = (width - gap * (n - 1)) / n;
  const values = days.map(measure);
  const max = Math.max(1, ...values);
  const lastIndex = days.length - 1;

  return (
    <div ref={hostRef} className="bars">
      <svg
        className="chart"
        viewBox={`0 0 ${width} ${chartH + labelH}`}
        width={width}
        height={chartH + labelH}
        role="img"
        aria-label={label}
      >
        <title>{label}</title>
        <line x1={0} x2={width} y1={chartH + 0.5} y2={chartH + 0.5} className="bars__base" />
        {days.map((d, i) => {
          const v = values[i];
          const h = v > 0 ? Math.max(3, (v / max) * (chartH - 4)) : 2;
          const x = i * (barW + gap);
          const cls =
            v === 0
              ? "bars__bar bars__bar--empty"
              : i === lastIndex
                ? "bars__bar bars__bar--today"
                : "bars__bar";
          const date = parseDay(d.date);
          const showLabel = i === 0 || i === lastIndex || i === Math.floor(lastIndex / 2);
          return (
            <g key={d.date}>
              <rect className={cls} x={x} y={chartH - h} width={barW} height={h} rx={3}>
                <title>{`${formatShortDay(date, locale)}: ${v}`}</title>
              </rect>
              {showLabel && (
                <text
                  className="bars__label"
                  x={x + barW / 2}
                  y={chartH + labelH - 2}
                  textAnchor={i === 0 ? "start" : i === lastIndex ? "end" : "middle"}
                >
                  {formatShortDay(date, locale)}
                </text>
              )}
            </g>
          );
        })}
      </svg>
    </div>
  );
}

type SparkProps = {
  values: number[];
  label: string;
  height?: number;
};

export function Sparkline({ values, label, height = 22 }: SparkProps) {
  const width = 100;
  const n = values.length;
  if (n < 2) {
    return null;
  }
  const max = Math.max(1, ...values);
  const pts = values.map((v, i) => {
    const x = (i / (n - 1)) * width;
    const y = height - 2 - (v / max) * (height - 4);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  });
  const line = pts.join(" ");
  const area = `0,${height} ${line} ${width},${height}`;
  return (
    <svg
      className="chart"
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      role="img"
      aria-label={label}
      style={{ height }}
    >
      <title>{label}</title>
      <polygon className="spark__area" points={area} />
      <polyline className="spark__line" points={line} vectorEffect="non-scaling-stroke" />
    </svg>
  );
}
