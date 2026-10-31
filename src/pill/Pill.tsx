import { AlertTriangle, Check, MicOff } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { I18nContext, resolveLocale, type Strings, strings } from "../i18n";
import { commands, type Notice, type PillState, type UiLanguage, useEvent } from "../ipc";
import { formatClock } from "../lib/format";
import { deadlineFrom, elapsedAt, fractionLeft, smoothLevel, startFrom } from "./countdown";

const BAR_COUNT = 16;
const CANCEL_TOTAL_MS = 3000;

function usePrefersReducedMotion(): boolean {
  const [reduced, setReduced] = useState(
    () => window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false,
  );
  useEffect(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    const onChange = () => setReduced(mq.matches);
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, []);
  return reduced;
}

/** Live waveform: a ring of smoothed levels, newest on the right. */
function useWaveform(active: boolean, reducedMotion: boolean): number[] {
  const [bars, setBars] = useState<number[]>(() => new Array<number>(BAR_COUNT).fill(0));
  const smoothed = useRef(0);
  const pending = useRef<number[]>(new Array<number>(BAR_COUNT).fill(0));
  const frame = useRef(0);

  useEvent("audioLevel", ({ level }) => {
    if (!active) {
      return;
    }
    smoothed.current = smoothLevel(smoothed.current, level);
    pending.current = [...pending.current.slice(1), smoothed.current];
    if (reducedMotion) {
      return;
    }
    if (!frame.current) {
      frame.current = window.requestAnimationFrame(() => {
        frame.current = 0;
        setBars(pending.current);
      });
    }
  });

  // Reduced motion: a calm update four times a second instead of a flowing wave.
  useEffect(() => {
    if (!active || !reducedMotion) {
      return;
    }
    const id = window.setInterval(() => {
      const level = smoothed.current;
      setBars(new Array<number>(BAR_COUNT).fill(level));
    }, 250);
    return () => window.clearInterval(id);
  }, [active, reducedMotion]);

  useEffect(() => {
    if (!active) {
      smoothed.current = 0;
    }
    return () => {
      if (frame.current) {
        window.cancelAnimationFrame(frame.current);
        frame.current = 0;
      }
    };
  }, [active]);

  return bars;
}

function Waveform({ bars, shimmer }: { bars: number[]; shimmer: boolean }) {
  return (
    <span className={shimmer ? "pill__wave pill__wave--shimmer" : "pill__wave"} aria-hidden="true">
      {bars.map((level, i) => (
        <span
          // The ring has a fixed length; index is the identity of a slot.
          key={i.toString()}
          className="pill__bar"
          style={{ height: `${Math.round(3 + level * 17)}px` }}
        />
      ))}
    </span>
  );
}

function Ring({ fraction }: { fraction: number }) {
  const r = 8;
  const c = 2 * Math.PI * r;
  return (
    <svg className="pill__ring" viewBox="0 0 20 20" aria-hidden="true">
      <circle className="pill__ring-track" cx="10" cy="10" r={r} />
      <circle
        className="pill__ring-arc"
        cx="10"
        cy="10"
        r={r}
        strokeDasharray={c}
        strokeDashoffset={c * (1 - fraction)}
      />
    </svg>
  );
}

function noticeContent(
  notice: Notice,
  t: Strings,
): { icon: "ok" | "warn" | "info"; text: string; sub?: string } {
  switch (notice.kind) {
    case "delivered":
      switch (notice.outcome.kind) {
        case "pasted":
          return { icon: "ok", text: t.pill.pasted };
        case "copied":
          return { icon: "ok", text: t.pill.copiedReason[notice.outcome.reason] };
        case "unconfirmed":
          return { icon: "info", text: t.pill.unconfirmed };
      }
      break;
    case "no_speech":
      return { icon: "info", text: t.pill.noSpeech };
    case "failed":
      return { icon: "warn", text: t.pill.failure[notice.failure], sub: t.pill.savedToHistory };
    case "capture_failed":
      return { icon: "warn", text: t.pill.capture[notice.error] };
  }
  return { icon: "info", text: "" };
}

function NoticeIcon({ tone }: { tone: "ok" | "warn" | "info" }) {
  if (tone === "ok") {
    return <Check className="pill__icon pill__icon--ok" aria-hidden="true" />;
  }
  if (tone === "warn") {
    return <AlertTriangle className="pill__icon pill__icon--warn" aria-hidden="true" />;
  }
  return <MicOff className="pill__icon pill__icon--info" aria-hidden="true" />;
}

export default function Pill() {
  const [state, setState] = useState<PillState>({ view: { kind: "hidden" }, language: "ja" });
  const [uiLanguage, setUiLanguage] = useState<UiLanguage>("system");
  const reducedMotion = usePrefersReducedMotion();
  const t = strings[resolveLocale(uiLanguage)];

  useEffect(() => {
    commands
      .getPillState()
      .then(setState)
      .catch(() => undefined);
    commands
      .getSettings()
      .then((s) => setUiLanguage(s.ui_language))
      .catch(() => undefined);
  }, []);
  useEvent("pillState", setState);
  useEvent("settingsChanged", ({ settings }) => setUiLanguage(settings.ui_language));

  const view = state.view;
  const recording = view.kind === "recording";
  const busy = view.kind === "transcribing" || view.kind === "delivering";
  const bars = useWaveform(recording, reducedMotion);

  // Elapsed time: resynced on every recording event, ticked locally.
  const startRef = useRef(performance.now());
  const [elapsedMs, setElapsedMs] = useState(0);
  useEffect(() => {
    if (view.kind !== "recording") {
      return;
    }
    startRef.current = startFrom(view.elapsed_ms, performance.now());
    setElapsedMs(view.elapsed_ms);
    const id = window.setInterval(
      () => setElapsedMs(elapsedAt(startRef.current, performance.now())),
      200,
    );
    return () => window.clearInterval(id);
  }, [view]);

  // Countdown: a deadline from `remaining_ms`, read locally.
  const [fraction, setFraction] = useState(1);
  useEffect(() => {
    if (view.kind !== "cancel_pending") {
      return;
    }
    const deadline = deadlineFrom(view.remaining_ms, performance.now());
    setFraction(fractionLeft(deadline, CANCEL_TOTAL_MS, performance.now()));
    const id = window.setInterval(
      () => setFraction(fractionLeft(deadline, CANCEL_TOTAL_MS, performance.now())),
      reducedMotion ? 250 : 40,
    );
    return () => window.clearInterval(id);
  }, [view, reducedMotion]);

  const notice = view.kind === "notice" ? noticeContent(view.notice, t) : null;
  const layoutKey = useMemo(() => {
    if (view.kind === "notice") {
      return `${view.kind}:${notice?.text}:${notice?.sub ?? ""}`;
    }
    if (view.kind === "cancel_pending") {
      return `${view.kind}:${t.pill.escToUndo}`;
    }
    return `${view.kind}:${state.language}`;
  }, [view, notice, state.language, t]);

  // Width: measured from the content and sent to the native window on every layout change.
  const contentRef = useRef<HTMLSpanElement>(null);
  const lastWidth = useRef(0);
  // biome-ignore lint/correctness/useExhaustiveDependencies: layoutKey names every change that moves the content width
  useLayoutEffect(() => {
    if (view.kind === "hidden") {
      lastWidth.current = 0;
      return;
    }
    if (!contentRef.current) {
      return;
    }
    const width = Math.ceil(contentRef.current.getBoundingClientRect().width);
    if (width > 0 && width !== lastWidth.current) {
      lastWidth.current = width;
      commands.setPillWidth(width).catch(() => undefined);
    }
  }, [layoutKey]);

  if (view.kind === "hidden") {
    return null;
  }

  const langTitle = state.language === "ja" ? t.common.japanese : t.common.english;

  return (
    <I18nContext.Provider value={t}>
      <button
        type="button"
        className="pill-capsule"
        onClick={() => commands.pillClicked().catch(() => undefined)}
        aria-label={
          recording
            ? `${t.pill.recording} ${formatClock(elapsedMs)}`
            : view.kind === "cancel_pending"
              ? t.pill.escToUndo
              : busy
                ? t.pill.transcribing
                : (notice?.text ?? "")
        }
      >
        <span ref={contentRef} className="pill">
          {(recording || busy) && (
            <>
              <span className="pill__dot" style={busy ? { visibility: "hidden" } : undefined} />
              <Waveform bars={bars} shimmer={busy} />
              <span className="pill__time">{formatClock(elapsedMs)}</span>
              <span className="pill__lang" title={langTitle}>
                {state.language === "ja" ? "あ" : "A"}
              </span>
            </>
          )}
          {view.kind === "cancel_pending" && (
            <>
              <Ring fraction={fraction} />
              <span className="pill__text">{t.pill.escToUndo}</span>
            </>
          )}
          {notice && (
            <span className="pill__text">
              <NoticeIcon tone={notice.icon} />
              <span>{notice.text}</span>
              {notice.sub && <span className="pill__sub">{notice.sub}</span>}
            </span>
          )}
        </span>
      </button>
    </I18nContext.Provider>
  );
}
