import { useEffect, useRef } from "react";
import { useT } from "../i18n";
import { Button } from "./Button";

type Props = {
  open: boolean;
  title: string;
  body: string;
  confirmLabel: string;
  destructive?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
};

/** In-app confirmation sheet. Never `window.confirm`. Esc cancels, the cancel button has focus. */
export function ConfirmSheet({
  open,
  title,
  body,
  confirmLabel,
  destructive = false,
  onConfirm,
  onCancel,
}: Props) {
  const t = useT();
  const cancelRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) {
      return;
    }
    cancelRef.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onCancel();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onCancel]);

  if (!open) {
    return null;
  }

  return (
    <div className="sheet-backdrop">
      <div className="sheet" role="alertdialog" aria-modal="true" aria-labelledby="sheet-title">
        <p className="sheet__title" id="sheet-title">
          {title}
        </p>
        <p className="sheet__body">{body}</p>
        <div className="sheet__actions">
          <Button ref={cancelRef} onClick={onCancel}>
            {t.common.cancel}
          </Button>
          <Button variant={destructive ? "danger-primary" : "primary"} onClick={onConfirm}>
            {confirmLabel}
          </Button>
        </div>
      </div>
    </div>
  );
}
