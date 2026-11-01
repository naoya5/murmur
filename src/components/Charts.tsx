// Hand-drawn SVG; the app adds no chart library.
import { useId, useLayoutEffect, useRef, useState } from "react";
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

type Size = { width: number; height: number };

function useMeasuredSize<T extends HTMLElement>(fallback: Size): [React.RefObject<T | null>, Size] {
  const ref = useRef<T>(null);
  const [size, setSize] = useState(fallback);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) {
      return;
    }
    const update = () => {
      const box = el.getBoundingClientRect();
      const width = Math.max(1, Math.round(box.width));
      const height = Math.max(1, Math.round(box.height));
      setSize((old) => (old.width === width && old.height === height ? old : { width, height }));
    };
    update();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(update);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);
  return [ref, size];
}

type TrendProps = {
  days: DayPoint[];
  locale: Locale;
  label: string;
  /** What each point measures; sessions by default. */
  measure?: (d: DayPoint) => number;
};

/**
 * A line of light over an amber haze. One point is lit: the hovered day, or today when the
 * pointer is away, with a dashed cursor and a glass tip that shows its value.
 */
export function Trend({ days, locale, label, measure = (d) => d.sessions }: TrendProps) {
  // The host's size comes from the layout (it grows with the hero on a tall window); the SVG
  // is drawn to fill it.
  const [hostRef, size] = useMeasuredSize<HTMLDivElement>({ width: 480, height: 128 });
  const width = size.width;
  const [hover, setHover] = useState<number | null>(null);
  const hazeId = `${useId().replace(/:/g, "")}-haze`;
  const glowId = `${hazeId}-glow`;

  const labelH = 18;
  const chartH = Math.max(64, size.height - labelH);
  const padTop = 14;
  const padX = 8;
  const n = days.length;
  if (n === 0) {
    return null;
  }
  const values = days.map(measure);
  const max = Math.max(1, ...values);
  const span = Math.max(1, width - padX * 2);
  const xAt = (i: number) => (n === 1 ? width / 2 : padX + (i / (n - 1)) * span);
  const yAt = (v: number) => padTop + (1 - v / max) * (chartH - padTop - 3);
  const points = values.map((v, i) => `${xAt(i).toFixed(1)},${yAt(v).toFixed(1)}`).join(" ");
  const area = `${xAt(0).toFixed(1)},${chartH} ${points} ${xAt(n - 1).toFixed(1)},${chartH}`;

  const lastIndex = n - 1;
  const active = hover ?? lastIndex;
  const activeX = xAt(active);
  const activeY = yAt(values[active] ?? 0);
  const activeDay = days[active];
  // The tip sits above the point, or below it when the point is near the top edge.
  const tipBelow = activeY < 48;
  const tipX = Math.min(Math.max(activeX, 34), width - 34);

  const onPointerMove = (e: React.PointerEvent<SVGSVGElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const index = Math.round(((e.clientX - box.left - padX) / span) * (n - 1));
    setHover(Math.min(lastIndex, Math.max(0, index)));
  };

  return (
    <div ref={hostRef} className="trend">
      <svg
        className="chart"
        viewBox={`0 0 ${width} ${chartH + labelH}`}
        width={width}
        height={chartH + labelH}
        role="img"
        aria-label={label}
        onPointerMove={onPointerMove}
        onPointerLeave={() => setHover(null)}
      >
        <title>{label}</title>
        <defs>
          <linearGradient id={hazeId} x1="0" y1="0" x2="0" y2="1">
            <stop offset="0" stopColor="#ff9d4d" stopOpacity="0.2" />
            <stop offset="1" stopColor="#ff9d4d" stopOpacity="0" />
          </linearGradient>
          <filter id={glowId} x="-100%" y="-100%" width="300%" height="300%">
            <feGaussianBlur stdDeviation="3.5" />
          </filter>
        </defs>
        <line className="trend__grid" x1={0} x2={width} y1={chartH + 0.5} y2={chartH + 0.5} />
        <polygon points={area} fill={`url(#${hazeId})`} />
        <polyline className="trend__line" points={points} />
        <line className="trend__cursor" x1={activeX} x2={activeX} y1={4} y2={chartH} />
        <circle
          className="trend__halo"
          cx={activeX}
          cy={activeY}
          r={9}
          filter={`url(#${glowId})`}
        />
        <circle className="trend__ring" cx={activeX} cy={activeY} r={7} />
        <circle className="trend__point" cx={activeX} cy={activeY} r={3.5} />
        {days.map((d, i) => {
          const showLabel = i === 0 || i === lastIndex || i === Math.floor(lastIndex / 2);
          if (!showLabel) {
            return null;
          }
          return (
            <text
              key={d.date}
              className="trend__label"
              x={xAt(i)}
              y={chartH + labelH - 3}
              textAnchor={i === 0 ? "start" : i === lastIndex ? "end" : "middle"}
            >
              {formatShortDay(parseDay(d.date), locale)}
            </text>
          );
        })}
      </svg>
      {activeDay && (
        <div
          className={`trend__tip${tipBelow ? " trend__tip--below" : ""}`}
          style={{ left: tipX, top: activeY }}
          aria-hidden="true"
        >
          <span className="trend__tip-value">{values[active]}</span>
          <span className="trend__tip-date">
            {formatShortDay(parseDay(activeDay.date), locale)}
          </span>
        </div>
      )}
    </div>
  );
}
