import { useEffect, useRef, useState } from "react";
import { useT } from "../i18n";
import {
  type Combo,
  comboFromEvent,
  comboTokens,
  hasModifier,
  isComplete,
  parseAccelerator,
  toAccelerator,
} from "../lib/hotkey";

type Props = {
  value: string | null;
  /** Called with the new accelerator, or null when the user cleared it (only if `clearable`). */
  onChange: (accelerator: string | null) => void;
  /** Called when recording starts; the app suspends the global hotkeys here. */
  onListenStart?: () => void;
  /** Called when recording ends, for any reason; the app resumes the hotkeys here. */
  onListenEnd?: () => void;
  clearable?: boolean;
  label: string;
};

export function Kbd({ tokens }: { tokens: string[] }) {
  return (
    <span className="kbd">
      {tokens.map((token, i) => (
        <span key={`${token}-${i.toString()}`} className="kbd__key">
          {token}
        </span>
      ))}
    </span>
  );
}

/**
 * Click to listen, press a combo, done. Esc cancels; Backspace clears when clearable.
 * Uses `event.code` so Option+letter does not turn into a symbol.
 */
export function KeyRecorder({
  value,
  onChange,
  onListenStart,
  onListenEnd,
  clearable = false,
  label,
}: Props) {
  const t = useT();
  const [listening, setListening] = useState(false);
  const [pending, setPending] = useState<Combo | null>(null);
  const [hint, setHint] = useState<string | null>(null);
  const startRef = useRef(onListenStart);
  startRef.current = onListenStart;
  const endRef = useRef(onListenEnd);
  endRef.current = onListenEnd;

  useEffect(() => {
    if (!listening) {
      return;
    }
    startRef.current?.();
    return () => {
      endRef.current?.();
    };
  }, [listening]);

  const stop = () => {
    setListening(false);
    setPending(null);
    setHint(null);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLButtonElement>) => {
    if (!listening) {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        setListening(true);
      }
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    if (e.code === "Escape") {
      stop();
      return;
    }
    if (e.code === "Backspace" && clearable && !hasModifier(comboFromEvent(e))) {
      onChange(null);
      stop();
      return;
    }
    const combo = comboFromEvent(e);
    if (!combo.key) {
      setPending(combo);
      setHint(null);
      return;
    }
    if (!isComplete(combo)) {
      setPending(combo);
      setHint(t.hotkey.modifiersOnly);
      return;
    }
    onChange(toAccelerator(combo));
    stop();
  };

  const onKeyUp = (e: React.KeyboardEvent<HTMLButtonElement>) => {
    if (listening && pending && !pending.key) {
      // All modifiers released without a key: keep listening, show the hint.
      if (!e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) {
        setPending(null);
        setHint(t.hotkey.modifiersOnly);
      }
    }
  };

  const shown = listening
    ? pending && (pending.key || hasModifier(pending))
      ? comboTokens(pending)
      : []
    : value
      ? comboTokens(parseAccelerator(value))
      : [];

  return (
    <span
      style={{ display: "inline-flex", flexDirection: "column", alignItems: "flex-end", gap: 3 }}
    >
      <button
        type="button"
        className={`keyrec${listening ? " keyrec--listening" : ""}`}
        aria-label={label}
        aria-pressed={listening}
        onClick={() => {
          if (!listening) {
            setListening(true);
          }
        }}
        onKeyDown={onKeyDown}
        onKeyUp={onKeyUp}
        onBlur={() => {
          if (listening) {
            stop();
          }
        }}
      >
        {shown.length > 0 ? (
          <Kbd tokens={shown} />
        ) : (
          <span className={listening ? "" : "muted"}>
            {listening ? t.hotkey.pressKeys : t.hotkey.none}
          </span>
        )}
      </button>
      {listening && (
        <span className="caption muted" aria-live="polite">
          {hint ?? t.hotkey.escToCancel}
        </span>
      )}
    </span>
  );
}
