import type { ReactNode } from "react";
import type { Language } from "../ipc";

type Props = {
  tone?: "default" | "warn" | "ok" | "accent";
  children: ReactNode;
  title?: string;
};

export function Badge({ tone = "default", children, title }: Props) {
  const cls = tone === "default" ? "badge" : `badge badge--${tone}`;
  return (
    <span className={cls} title={title}>
      {children}
    </span>
  );
}

/** "あ" for Japanese, "A" for English. */
export function LangBadge({ language, title }: { language: Language; title: string }) {
  return (
    <span className="badge badge--lang" title={title} role="img" aria-label={title}>
      {language === "ja" ? "あ" : "A"}
    </span>
  );
}

export function Dot({ tone }: { tone: "ok" | "warn" | "bad" | "none" }) {
  return <span className={tone === "none" ? "dot" : `dot dot--${tone}`} aria-hidden="true" />;
}
