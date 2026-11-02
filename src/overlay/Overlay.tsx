import { useCallback, useEffect, useRef } from "react";
import { commands, type InkPoint, useEvent } from "../ipc";
import { appendPoint, isStroke, strokeAlpha, strokeGone, toUnit } from "./ink";

type Stroke = {
  points: InkPoint[];
  /** `Date.now()` when the pointer was released; null while the stroke is still being drawn. */
  endedAt: number | null;
};

type Palette = { line: string; edge: string };

const LINE_WIDTH = 4;
const EDGE_WIDTH = 7.5;

function readPalette(): Palette {
  const style = getComputedStyle(document.documentElement);
  return {
    line: style.getPropertyValue("--ink-stroke").trim() || "#ff3b30",
    edge: style.getPropertyValue("--ink-stroke-edge").trim() || "rgba(255, 255, 255, 0.92)",
  };
}

function trace(ctx: CanvasRenderingContext2D, points: InkPoint[], w: number, h: number): void {
  ctx.beginPath();
  points.forEach((p, i) => {
    if (i === 0) {
      ctx.moveTo(p.x * w, p.y * h);
    } else {
      ctx.lineTo(p.x * w, p.y * h);
    }
  });
}

/**
 * The drawing layer: a transparent full-screen canvas. The native side lets clicks through
 * except while the user holds the modifier key, so every pointer event that reaches this page
 * is meant to draw. A finished stroke is sent to the backend, then held and faded out here.
 */
export default function Overlay() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const shown = useRef(false);
  const strokes = useRef<Stroke[]>([]);
  const active = useRef<{ stroke: Stroke; startedMs: number } | null>(null);
  const frame = useRef(0);
  const palette = useRef<Palette>({ line: "#ff3b30", edge: "#fff" });

  const draw = useCallback(() => {
    frame.current = 0;
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) {
      return;
    }
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    ctx.clearRect(0, 0, w, h);
    const now = Date.now();
    strokes.current = strokes.current.filter(
      (s) => s.endedAt === null || !strokeGone(now - s.endedAt),
    );
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    for (const s of strokes.current) {
      if (s.points.length < 2) {
        continue;
      }
      ctx.globalAlpha = s.endedAt === null ? 1 : strokeAlpha(now - s.endedAt);
      trace(ctx, s.points, w, h);
      ctx.strokeStyle = palette.current.edge;
      ctx.lineWidth = EDGE_WIDTH;
      ctx.stroke();
      ctx.strokeStyle = palette.current.line;
      ctx.lineWidth = LINE_WIDTH;
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
    if (strokes.current.length > 0) {
      frame.current = window.requestAnimationFrame(draw);
    }
  }, []);

  const schedule = useCallback(() => {
    if (!frame.current) {
      frame.current = window.requestAnimationFrame(draw);
    }
  }, [draw]);

  const resize = useCallback(() => {
    const canvas = canvasRef.current;
    if (!canvas) {
      return;
    }
    const dpr = window.devicePixelRatio || 1;
    canvas.width = Math.round(canvas.clientWidth * dpr);
    canvas.height = Math.round(canvas.clientHeight * dpr);
    canvas.getContext("2d")?.setTransform(dpr, 0, 0, dpr, 0, 0);
    schedule();
  }, [schedule]);

  useEffect(() => {
    palette.current = readPalette();
    resize();
    window.addEventListener("resize", resize);
    return () => {
      window.removeEventListener("resize", resize);
      if (frame.current) {
        window.cancelAnimationFrame(frame.current);
        frame.current = 0;
      }
    };
  }, [resize]);

  // Ask for the current state once mounted: a recording may have started before this page.
  useEffect(() => {
    commands.overlayReady().catch(() => undefined);
  }, []);

  useEvent("overlayState", (state) => {
    if (state.kind === "shown") {
      shown.current = true;
      return;
    }
    shown.current = false;
    active.current = null;
    strokes.current = [];
    schedule();
  });

  const finish = useCallback(() => {
    const current = active.current;
    active.current = null;
    if (!current) {
      return;
    }
    const endedMs = Date.now();
    if (!isStroke(current.stroke.points)) {
      strokes.current = strokes.current.filter((s) => s !== current.stroke);
      schedule();
      return;
    }
    current.stroke.endedAt = endedMs;
    commands.submitInkStroke(current.stroke.points, current.startedMs, endedMs).catch(() => {
      // The drawing is only a hint; a lost stroke must not break the recording.
    });
    schedule();
  }, [schedule]);

  const onPointerDown = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!shown.current || active.current) {
      return;
    }
    const canvas = e.currentTarget;
    canvas.setPointerCapture(e.pointerId);
    const first = toUnit(e.clientX, e.clientY, canvas.clientWidth, canvas.clientHeight);
    const stroke: Stroke = { points: [first], endedAt: null };
    strokes.current.push(stroke);
    active.current = { stroke, startedMs: Date.now() };
    schedule();
  };

  const onPointerMove = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const current = active.current;
    if (!current) {
      return;
    }
    const canvas = e.currentTarget;
    const next = toUnit(e.clientX, e.clientY, canvas.clientWidth, canvas.clientHeight);
    if (appendPoint(current.stroke.points, next)) {
      schedule();
    }
  };

  return (
    <canvas
      ref={canvasRef}
      className="overlay-canvas"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={finish}
      onPointerCancel={finish}
    />
  );
}
