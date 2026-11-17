// Hand-drawn SVG; the app adds no chart library.
import { useId, useLayoutEffect, useMemo, useRef, useState } from "react";
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

const DEFAULT_MEASURE = (d: DayPoint) => d.sessions;

/** Smooth monotone-ish cubic path through points, clamped vertically so curves never dip below baseline. */
function smoothLinePath(xs: number[], ys: number[], minY: number, maxY: number): string {
  const n = xs.length;
  if (n === 0) {
    return "";
  }
  if (n === 1) {
    return `M${xs[0].toFixed(1)},${ys[0].toFixed(1)}`;
  }
  const clampY = (y: number) => Math.min(maxY, Math.max(minY, y));
  let d = `M${xs[0].toFixed(1)},${ys[0].toFixed(1)}`;
  for (let i = 0; i < n - 1; i++) {
    const x0 = xs[Math.max(0, i - 1)];
    const y0 = ys[Math.max(0, i - 1)];
    const x1 = xs[i];
    const y1 = ys[i];
    const x2 = xs[i + 1];
    const y2 = ys[i + 1];
    const x3 = xs[Math.min(n - 1, i + 2)];
    const y3 = ys[Math.min(n - 1, i + 2)];
    const tension = 0.22;
    const cp1x = x1 + (x2 - x0) * tension;
    const cp1y = clampY(y1 + (y2 - y0) * tension);
    const cp2x = x2 - (x3 - x1) * tension;
    const cp2y = clampY(y2 - (y3 - y1) * tension);
    d += ` C${cp1x.toFixed(1)},${cp1y.toFixed(1)} ${cp2x.toFixed(1)},${cp2y.toFixed(1)} ${x2.toFixed(1)},${y2.toFixed(1)}`;
  }
  return d;
}

/**
 * A line of light over an amber haze. One point is lit: the hovered day, or today when the
 * pointer is away, with a dashed cursor and a glass tip that shows its value.
 */
export function Trend({ days, locale, label, measure = DEFAULT_MEASURE }: TrendProps) {
  // The host's size comes from the layout (it grows with the hero on a tall window); the SVG
  // is drawn to fill it.
  const [hostRef, size] = useMeasuredSize<HTMLDivElement>({ width: 480, height: 100 });
  const width = size.width;
  const [hover, setHover] = useState<number | null>(null);
  const hazeId = `${useId().replace(/:/g, "")}-haze`;
  const glowId = `${hazeId}-glow`;

  const geometry = useMemo(() => {
    const labelH = 18;
    const chartH = Math.max(56, size.height - labelH);
    const padTop = 12;
    const padBottom = 3;
    const padX = 8;
    const n = days.length;
    if (n === 0) {
      return null;
    }
    const values = days.map(measure);
    const max = Math.max(1, ...values);
    const span = Math.max(1, width - padX * 2);
    const xAt = (i: number) => (n === 1 ? width / 2 : padX + (i / (n - 1)) * span);
    const yAt = (v: number) => padTop + (1 - v / max) * (chartH - padTop - padBottom);
    const xs = days.map((_, i) => xAt(i));
    const ys = values.map(yAt);
    const linePath = smoothLinePath(xs, ys, padTop, chartH - padBottom);
    const areaPath = `${linePath} L${xs[n - 1].toFixed(1)},${chartH} L${xs[0].toFixed(1)},${chartH} Z`;
    const midY = (padTop + chartH - padBottom) / 2;
    const lastIndex = n - 1;
    const midIndex = Math.floor(lastIndex / 2);
    const labels: {
      date: string;
      x: number;
      text: string;
      anchor: "start" | "end" | "middle";
    }[] = [];
    for (let i = 0; i < n; i++) {
      if (i === 0 || i === lastIndex || i === midIndex) {
        labels.push({
          date: days[i].date,
          x: xs[i],
          text: formatShortDay(parseDay(days[i].date), locale),
          anchor: i === 0 ? "start" : i === lastIndex ? "end" : "middle",
        });
      }
    }
    return {
      labelH,
      chartH,
      midY,
      padX,
      span,
      lastIndex,
      values,
      xs,
      ys,
      linePath,
      areaPath,
      labels,
    };
  }, [days, locale, measure, size.height, width]);

  if (!geometry) {
    return null;
  }

  const {
    labelH,
    chartH,
    midY,
    padX,
    span,
    lastIndex,
    values,
    xs,
    ys,
    linePath,
    areaPath,
    labels,
  } = geometry;
  const active = hover ?? lastIndex;
  const activeX = xs[active] ?? xs[lastIndex];
  const activeY = ys[active] ?? ys[lastIndex];
  const activeDay = days[active];
  // Place the tip vertically unless mid-height dots at the right edge would collide with axis/meta labels.
  const tipSideLeft = activeX > width - 76 && activeY >= 24 && activeY <= chartH - 20;
  const tipBelow = !tipSideLeft && activeY < 28;
  const tipX = tipSideLeft ? activeX : Math.min(Math.max(activeX, 40), width - 40);

  const onPointerMove = (e: React.PointerEvent<SVGSVGElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const index = Math.round(((e.clientX - box.left - padX) / span) * lastIndex);
    const next = Math.min(lastIndex, Math.max(0, index));
    setHover((prev) => (prev === next ? prev : next));
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
            <stop offset="0" stopColor="#ff9d4d" stopOpacity="0.24" />
            <stop offset="1" stopColor="#ff9d4d" stopOpacity="0" />
          </linearGradient>
          <filter id={glowId} x="-100%" y="-100%" width="300%" height="300%">
            <feGaussianBlur stdDeviation="3.5" />
          </filter>
        </defs>
        <line className="trend__grid trend__grid--mid" x1={0} x2={width} y1={midY} y2={midY} />
        <line className="trend__grid" x1={0} x2={width} y1={chartH + 0.5} y2={chartH + 0.5} />
        <path d={areaPath} fill={`url(#${hazeId})`} />
        <path className="trend__line" d={linePath} />
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
        {labels.map((item) => (
          <text
            key={item.date}
            className="trend__label"
            x={item.x}
            y={chartH + labelH - 3}
            textAnchor={item.anchor}
          >
            {item.text}
          </text>
        ))}
      </svg>
      {activeDay && (
        <div
          className={`trend__tip${tipSideLeft ? " trend__tip--left" : tipBelow ? " trend__tip--below" : ""}`}
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
