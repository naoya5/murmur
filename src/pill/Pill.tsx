import { AlertTriangle, Camera, Check, MicOff } from "lucide-react";
import {
  type CSSProperties,
  memo,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { I18nContext, resolveLocale, type Strings, strings } from "../i18n";
import {
  type AnnotationPill,
  commands,
  type DeliveryOutcome,
  type Notice,
  type PillState,
  type UiLanguage,
  useEvent,
} from "../ipc";
import { formatClock } from "../lib/format";
import { deadlineFrom, elapsedAt, fractionLeft, smoothLevel, startFrom } from "./countdown";

const BAR_COUNT = 16;
/** Transcribing: the wave also takes the clock's place (see `.pill__wave--breathe`), with more bars. */
const BREATHE_BARS = Array.from({ length: 28 }, (_, i) => i);
const CANCEL_TOTAL_MS = 3000;
/** How long the drawing hint shows at the start of an annotation recording. */
const HINT_MS = 3000;

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

  // The next recording starts flat, not with the last one's levels.
  useEffect(() => {
    if (!active) {
      smoothed.current = 0;
      pending.current = new Array<number>(BAR_COUNT).fill(0);
      setBars(pending.current);
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

const NO_BARS: number[] = [];

/** The recording waveform. It owns the level state, so a level re-renders only the bars, not the
 *  whole pill (the parent also re-renders for the clock). */
const LiveWaveform = memo(function LiveWaveform({ reducedMotion }: { reducedMotion: boolean }) {
  const bars = useWaveform(true, reducedMotion);
  return <Waveform bars={bars} breathe={false} />;
});

/** Recording: the live levels. Transcribing: every bar breathes, one step behind its left neighbour
 *  (the bar index `--i` sets the phase in styles.css). */
function Waveform({ bars, breathe }: { bars: number[]; breathe: boolean }) {
  if (breathe) {
    return (
      <span className="pill__wave pill__wave--breathe" aria-hidden="true">
        {BREATHE_BARS.map((i) => (
          <span key={i} className="pill__bar" style={{ "--i": i } as CSSProperties} />
        ))}
      </span>
    );
  }
  return (
    <span className="pill__wave" aria-hidden="true">
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

/** The light that circles the capsule while it transcribes: a soft glow outside, a bright rim on the
 *  edge. The halo goes under the capsule, the rim over it. */
function Glow({ layer }: { layer: "halo" | "rim" }) {
  if (layer === "halo") {
    return (
      <span className="pill-glow" aria-hidden="true">
        <span className="pill-glow__band">
          <span className="pill-glow__orbit" />
        </span>
      </span>
    );
  }
  return (
    <span className="pill-rim" aria-hidden="true">
      <span className="pill-rim__orbit" />
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

function annotationDelivered(
  outcome: DeliveryOutcome,
  images: number,
  t: Strings,
): { icon: "ok" | "warn" | "info"; text: string; sub?: string } {
  const a = t.pill.annotation;
  const wordsOnly = images === 0;
  if (outcome.kind === "unconfirmed") {
    return { icon: "info", text: t.pill.unconfirmed };
  }
  const pasted = outcome.kind === "pasted";
  const text = wordsOnly
    ? pasted
      ? a.pastedWordsOnly
      : a.copiedWordsOnly
    : pasted
      ? a.pastedWithImages(images)
      : a.copiedWithImages(images);
  let sub: string | undefined;
  if (wordsOnly) {
    sub = a.imagesInFolder;
  } else if (outcome.kind === "copied") {
    sub =
      outcome.reason === "secure_input"
        ? a.secureField
        : outcome.reason === "target_not_in_front"
          ? a.appNotInFront
          : a.pressPaste;
  }
  return { icon: wordsOnly ? "warn" : "ok", text, sub };
}

export function noticeContent(
  notice: Notice,
  t: Strings,
): { icon: "ok" | "warn" | "info"; text: string; sub?: string } {
  switch (notice.kind) {
    case "delivered":
      switch (notice.outcome.kind) {
        case "pasted":
          return { icon: "ok", text: t.pill.pasted };
        case "copied":
          return {
            // A failed translation is a failure, even though the Japanese is on the clipboard.
            icon: notice.outcome.reason === "refine_failed" ? "warn" : "ok",
            text: t.pill.copiedReason[notice.outcome.reason],
          };
        case "unconfirmed":
          return { icon: "info", text: t.pill.unconfirmed };
      }
      break;
    case "cleanup_skipped": {
      const delivered = noticeContent({ kind: "delivered", outcome: notice.outcome }, t);
      return {
        icon: "warn",
        text: t.pill.cleanupSkipped,
        sub: delivered.text,
      };
    }
    case "no_speech":
      return { icon: "info", text: t.pill.noSpeech };
    case "failed":
      return { icon: "warn", text: t.pill.failure[notice.failure], sub: t.pill.savedToHistory };
    case "capture_failed":
      return { icon: "warn", text: t.pill.capture[notice.error] };
    case "annotation_delivered":
      return annotationDelivered(notice.outcome, notice.images, t);
    case "annotation_empty":
      return { icon: "info", text: t.pill.annotation.empty };
    case "annotations_refused":
      return { icon: "warn", text: t.pill.annotation.refused[notice.reason] };
  }
  return { icon: "info", text: "" };
}

function ScreenMark({ annotation, t }: { annotation: AnnotationPill; t: Strings }) {
  const lost = annotation.screen_lost;
  return (
    <span
      className={lost ? "pill__shot pill__shot--lost" : "pill__shot"}
      title={lost ? t.pill.annotation.screenLost : t.pill.annotation.screen}
    >
      {lost ? <AlertTriangle aria-hidden="true" /> : <Camera aria-hidden="true" />}
      <span>{annotation.images}</span>
    </span>
  );
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
  const busy =
    view.kind === "transcribing" || view.kind === "refining" || view.kind === "delivering";

  // Elapsed time: resynced on every recording event, ticked locally. Synced before paint, so a new
  // recording never shows the last one's time for a frame.
  const startRef = useRef(performance.now());
  const [elapsedMs, setElapsedMs] = useState(0);
  useLayoutEffect(() => {
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

  const annotation = state.annotation ?? null;
  const showHint =
    recording && annotation !== null && !annotation.screen_lost && elapsedMs < HINT_MS;
  const notice = view.kind === "notice" ? noticeContent(view.notice, t) : null;
  const layoutKey = useMemo(() => {
    if (view.kind === "notice") {
      return `${view.kind}:${notice?.text}:${notice?.sub ?? ""}`;
    }
    if (view.kind === "cancel_pending") {
      return `${view.kind}:${t.pill.escToUndo}`;
    }
    const task = view.kind === "refining" ? view.task : "";
    return `${view.kind}:${task}:${state.language}:${state.refine ?? ""}:${annotation ? `shot${annotation.images}${annotation.screen_lost}${showHint}` : ""}`;
  }, [view, notice, state.language, state.refine, t, annotation, showHint]);

  // Width: measured from the content on every layout change and sent to the native side, which cuts
  // the glass to it. The page's capsule changes at the next paint, the glass at once. The glass must
  // never stand out past the capsule (a bare gray slab), while the capsule's fill past the glass for
  // a frame is hard to see: a narrower capsule is sent now, a wider one (the first one of a showing
  // included: the glass waits at zero width) after the next paint.
  const contentRef = useRef<HTMLSpanElement>(null);
  const sentWidth = useRef(0);
  // biome-ignore lint/correctness/useExhaustiveDependencies: layoutKey names every change that moves the content width
  useLayoutEffect(() => {
    if (view.kind === "hidden") {
      sentWidth.current = 0;
      return;
    }
    if (!contentRef.current) {
      return;
    }
    const width = Math.ceil(contentRef.current.getBoundingClientRect().width);
    if (width <= 0 || width === sentWidth.current) {
      return;
    }
    const send = () => {
      sentWidth.current = width;
      commands.setPillWidth(width).catch(() => undefined);
    };
    if (width < sentWidth.current) {
      send();
      return;
    }
    let afterPaint = 0;
    const beforePaint = window.requestAnimationFrame(() => {
      afterPaint = window.requestAnimationFrame(send);
    });
    return () => {
      window.cancelAnimationFrame(beforePaint);
      window.cancelAnimationFrame(afterPaint);
    };
  }, [layoutKey]);

  if (view.kind === "hidden") {
    return null;
  }

  const langTitle = state.language === "ja" ? t.common.japanese : t.common.english;
  const translating = state.refine === "translate";
  const busyLabel = view.kind === "refining" ? t.pill.refining[view.task] : t.pill.transcribing;

  const contentClass =
    recording || busy ? "pill pill--wave" : notice ? "pill pill--notice" : "pill";

  return (
    <I18nContext.Provider value={t}>
      <div className={busy ? "pill-frame pill-frame--glow" : "pill-frame"}>
        {busy && <Glow layer="halo" />}
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
                  ? busyLabel
                  : (notice?.text ?? "")
          }
        >
          <span ref={contentRef} className={contentClass}>
            {(recording || busy) && (
              <>
                <span className="pill__well">
                  {recording && <span className="pill__dot" />}
                  {recording ? (
                    <LiveWaveform reducedMotion={reducedMotion} />
                  ) : (
                    <Waveform bars={NO_BARS} breathe={busy} />
                  )}
                </span>
                {annotation && <ScreenMark annotation={annotation} t={t} />}
                {recording && <span className="pill__time">{formatClock(elapsedMs)}</span>}
                {translating ? (
                  <span className="pill__lang pill__lang--pair" title={t.pill.translateMark}>
                    JA → EN
                  </span>
                ) : (
                  <span className="pill__lang" title={langTitle}>
                    {state.language === "ja" ? "あ" : "A"}
                  </span>
                )}
                {view.kind === "refining" && (
                  <span className="pill__hint">{t.pill.refining[view.task]}</span>
                )}
                {recording && annotation?.screen_lost && (
                  <span className="pill__hint pill__hint--warn">
                    {t.pill.annotation.screenLost}
                  </span>
                )}
                {showHint && <span className="pill__hint">{t.pill.annotation.hint}</span>}
              </>
            )}
            {view.kind === "cancel_pending" && (
              <>
                <Ring fraction={fraction} />
                <span className="pill__text">{t.pill.escToUndo}</span>
              </>
            )}
            {notice && (
              <>
                <span className="pill__well pill__well--icon">
                  <NoticeIcon tone={notice.icon} />
                </span>
                <span className="pill__text">
                  <span>{notice.text}</span>
                  {notice.sub && <span className="pill__sub">{notice.sub}</span>}
                </span>
              </>
            )}
          </span>
        </button>
        {busy && <Glow layer="rim" />}
      </div>
    </I18nContext.Provider>
  );
}
