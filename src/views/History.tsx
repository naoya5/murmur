import {
  ChevronLeft,
  ChevronRight,
  Clock,
  Copy,
  RotateCcw,
  Search,
  Trash2,
  Volume2,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Badge, LangBadge } from "../components/Badge";
import { Button } from "../components/Button";
import { EmptyState } from "../components/EmptyState";
import { Card } from "../components/Glass";
import { ConfirmSheet } from "../components/Sheet";
import { TextField } from "../components/TextField";
import { useToast } from "../components/Toast";
import { useT } from "../i18n";
import {
  commands,
  type FailureKind,
  type HistoryPage,
  type RetryError,
  type TranscriptRow,
  useEvent,
} from "../ipc";
import { formatClock, formatNumber, formatWhen } from "../lib/format";
import { clampOffset, hasNewer, hasOlder, PAGE_SIZE, pageRange } from "./historyPaging";

const FAILURES: FailureKind[] = [
  "timeout",
  "empty_result",
  "model_missing",
  "model_load_failed",
  "engine",
  "audio",
];

function failureText(error: string | null, t: ReturnType<typeof useT>): string {
  const kind = FAILURES.find((k) => k === error);
  return kind ? t.history.failure[kind] : (error ?? t.common.failed);
}

function retryErrorText(error: RetryError, t: ReturnType<typeof useT>): string {
  if (error.kind === "failed") {
    return `${t.history.retryFailed}: ${error.message}`;
  }
  return t.history.retryError[error.kind];
}

function needsClamp(text: string): boolean {
  return text.length > 110 || text.includes("\n");
}

type ItemProps = {
  row: TranscriptRow;
  onRetried: (row: TranscriptRow) => void;
  onDelete: (id: string) => void;
};

const WAVE_BARS = [6, 11, 15, 9, 14, 17, 11, 8, 15, 10, 14, 8, 16, 12, 7, 11, 13, 9].map(
  (h, idx) => ({ id: `wb-${idx}`, h, lit: idx < 12 }),
);

function Inspector({ row, onRetried, onDelete }: ItemProps) {
  const t = useT();
  const toast = useToast();
  const [expanded, setExpanded] = useState(false);
  const [showBefore, setShowBefore] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const [retryError, setRetryError] = useState<string | null>(null);

  // Reset per-row disclosures when switching selection in the list pane.
  // biome-ignore lint/correctness/useExhaustiveDependencies: row.id drives reset on selection change
  useEffect(() => {
    setExpanded(false);
    setShowBefore(false);
    setRetryError(null);
  }, [row.id]);

  const copyText = async (text: string | null) => {
    if (!text) {
      return;
    }
    const result = await commands.copyText(text);
    if (result.status === "ok") {
      toast(t.common.copied);
    }
  };

  const retry = async () => {
    setRetrying(true);
    setRetryError(null);
    const result = await commands.retryHistoryItem(row.id);
    setRetrying(false);
    if (result.status === "ok") {
      onRetried(result.data);
    } else {
      setRetryError(retryErrorText(result.error, t));
    }
  };

  const failed = row.status === "failed";
  const text = row.text ?? "";
  const clamp = !expanded && needsClamp(text);
  const refine = row.refine_kind;
  const refined = refine !== null && row.refine_status === "ok";
  // Cleanup offers a different original for any status; translation keeps success-only access.
  const before = (refine === "clean" ? row.pre_refine_text !== text : refined)
    ? row.pre_refine_text
    : null;
  const refineFailed =
    refine !== null && (row.refine_status === "failed" || row.refine_status === "timeout");

  return (
    <Card className="hist-inspector">
      <div className="hist-inspector__head">
        <div className="hist__meta">
          {row.language ? (
            <LangBadge
              language={row.language}
              title={row.language === "ja" ? t.common.japanese : t.common.english}
            />
          ) : (
            <span className="badge badge--lang" aria-hidden="true">
              -
            </span>
          )}
          <span>{formatWhen(row.created_at_ms, t.locale)}</span>
          {failed && <Badge tone="warn">{t.history.statusFailed}</Badge>}
          {refine && refined && <Badge tone="accent">{t.history.refine[refine]}</Badge>}
          {refine === "clean" && row.refine_status === "skipped" && (
            <Badge>{t.history.refineSkipped}</Badge>
          )}
          {refine && refineFailed && <Badge tone="warn">{t.history.refineFailed[refine]}</Badge>}
          {row.status === "recovered" && <Badge tone="ok">{t.history.statusRecovered}</Badge>}
          {retrying && <span>{t.history.retrying}</span>}
        </div>
        <div className="hist-inspector__meta-right">
          {row.latency_ms !== null && (
            <span className="caption faint tnum">
              {`${t.history.latencyLabel}: ${formatNumber(row.latency_ms, t.locale)}${t.units.ms}`}
            </span>
          )}
          <Button
            size="sm"
            variant="quiet"
            iconOnly
            className="btn--danger-hover"
            icon={<Trash2 aria-hidden="true" />}
            title={t.common.delete}
            aria-label={t.common.delete}
            onClick={() => onDelete(row.id)}
          />
        </div>
      </div>

      <div className="hist-inspector__body">
        {failed ? (
          <p className="hist__text hist__text--error">{failureText(row.error, t)}</p>
        ) : (
          <p
            className={`hist__text hist-inspector__text selectable${clamp ? " hist__text--clamp" : ""}`}
          >
            {text}
          </p>
        )}

        <div className="hist-inspector__actions">
          {failed ? (
            <Button
              size="sm"
              variant="primary"
              icon={<RotateCcw aria-hidden="true" />}
              onClick={retry}
              disabled={retrying}
            >
              {t.common.retry}
            </Button>
          ) : (
            <Button
              size="sm"
              variant="primary"
              icon={<Copy aria-hidden="true" />}
              aria-label={t.common.copy}
              onClick={() => copyText(row.text)}
            >
              {t.common.copy}
            </Button>
          )}
          {!failed && (needsClamp(text) || (refine && before)) && (
            <div className="hist__links">
              {needsClamp(text) && (
                <button
                  type="button"
                  className="hist__expand"
                  onClick={() => setExpanded((v) => !v)}
                >
                  {expanded ? t.history.collapse : t.history.expand}
                </button>
              )}
              {refine && before && (
                <button
                  type="button"
                  className="hist__expand"
                  aria-expanded={showBefore}
                  onClick={() => setShowBefore((v) => !v)}
                >
                  {showBefore ? t.history.hideBefore[refine] : t.history.showBefore[refine]}
                </button>
              )}
            </div>
          )}
        </div>

        {!failed && row.duration_ms > 0 && (
          <div className="hist-inspector__wave" aria-hidden="true">
            <Volume2 />
            <div className="hist-inspector__bars">
              {WAVE_BARS.map((bar) => (
                <span
                  key={bar.id}
                  className={bar.lit ? "hist-inspector__bar is-lit" : "hist-inspector__bar"}
                  style={{ height: `${bar.h}px` }}
                />
              ))}
            </div>
            <span className="caption muted tnum">{formatClock(row.duration_ms)}</span>
          </div>
        )}

        {refine && before && showBefore && (
          <div className="hist__before">
            <p className="hist__text selectable" lang="ja">
              {before}
            </p>
            <Button
              size="sm"
              variant="quiet"
              icon={<Copy aria-hidden="true" />}
              onClick={() => copyText(before)}
            >
              {t.history.copyBefore[refine]}
            </Button>
          </div>
        )}

        {retryError && (
          <p className="row__error" role="alert">
            {retryError}
          </p>
        )}
      </div>
    </Card>
  );
}

export function History() {
  const t = useT();
  const toast = useToast();
  const [query, setQuery] = useState("");
  const [debounced, setDebounced] = useState("");
  const [offset, setOffset] = useState(0);
  const [page, setPage] = useState<HistoryPage | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const requestId = useRef(0);

  useEffect(() => {
    const id = window.setTimeout(() => {
      setDebounced(query);
      setOffset(0);
    }, 150);
    return () => window.clearTimeout(id);
  }, [query]);

  const load = useCallback(() => {
    const id = ++requestId.current;
    commands
      .listHistory(debounced, PAGE_SIZE, offset)
      .then((result) => {
        if (id !== requestId.current) {
          return;
        }
        if (result.status === "ok") {
          const clamped = clampOffset(offset, result.data.total);
          if (clamped !== offset) {
            setOffset(clamped);
            return;
          }
          setPage(result.data);
          setError(null);
        } else {
          setError(result.error.message);
        }
      })
      .catch((e: unknown) => setError(String(e)));
  }, [debounced, offset]);

  useEffect(load, [load]);
  useEvent("historyChanged", load);

  const onRetried = (row: TranscriptRow) => {
    setPage((p) => (p ? { ...p, items: p.items.map((r) => (r.id === row.id ? row : r)) } : p));
  };

  const onDelete = async (id: string) => {
    const result = await commands.deleteHistoryItem(id);
    if (result.status === "ok") {
      toast(t.common.deleted);
      load();
    }
  };

  const clearAll = async () => {
    setConfirmClear(false);
    const result = await commands.clearHistory();
    if (result.status === "ok") {
      setOffset(0);
      load();
    }
  };

  const total = page?.total ?? 0;
  const range = pageRange(offset, page?.items.length ?? 0, total);
  const searching = debounced.trim().length > 0;
  const activeRow = page?.items.find((r) => r.id === selectedId) ?? page?.items[0] ?? null;

  return (
    <>
      <header className="view__header">
        <h1 className="view__title">{t.history.title}</h1>
        <div className="view__actions">
          <TextField
            label={t.history.search}
            type="search"
            placeholder={t.history.search}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            className="field--search"
            leading={<Search aria-hidden="true" />}
          />
          <Button
            variant="quiet"
            className="btn--danger-hover"
            onClick={() => setConfirmClear(true)}
            disabled={total === 0 && !searching}
          >
            {t.history.clearAll}
          </Button>
        </div>
      </header>

      {error && (
        <Card>
          <p className="muted">{t.history.loadError}</p>
          <p className="caption faint selectable">{error}</p>
        </Card>
      )}

      {page && total === 0 && !error && (
        <Card>
          {searching ? (
            <EmptyState icon={<Search />} title={t.history.noMatch} body={t.history.noMatchBody} />
          ) : (
            <EmptyState icon={<Clock />} title={t.history.emptyTitle} body={t.history.emptyBody} />
          )}
        </Card>
      )}

      {page && total > 0 && activeRow && (
        <div className="hist-split">
          <Card tight className="hist-split__list">
            <div className="hist-list">
              {page.items.map((row) => {
                const selected = row.id === activeRow.id;
                const rowFailed = row.status === "failed";
                const snippet = rowFailed ? failureText(row.error, t) : (row.text ?? "");
                const when = formatWhen(row.created_at_ms, t.locale);
                const langTag = row.language ? row.language.toUpperCase() : "-";
                return (
                  <button
                    key={row.id}
                    type="button"
                    className={selected ? "hist-row hist-row--active" : "hist-row"}
                    aria-current={selected ? "true" : undefined}
                    onClick={() => setSelectedId(row.id)}
                  >
                    <div className="hist-row__top">
                      <span className="hist-row__when">{langTag}</span>
                      <span
                        className={`hist-row__dot${rowFailed ? " is-warn" : row.refine_kind ? " is-accent" : ""}`}
                        aria-hidden="true"
                      />
                    </div>
                    <p
                      className={`hist-row__snippet${rowFailed ? " hist-row__snippet--error" : ""}`}
                    >
                      {`${when} · ${snippet}`}
                    </p>
                  </button>
                );
              })}
            </div>
            <div className="pager hist-split__pager">
              <span>{t.history.pageOf(range.from, range.to, total)}</span>
              <span className="pager__buttons">
                <Button
                  size="sm"
                  iconOnly
                  icon={<ChevronLeft aria-hidden="true" />}
                  title={t.history.prev}
                  aria-label={t.history.prev}
                  disabled={!hasNewer(offset)}
                  onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
                />
                <Button
                  size="sm"
                  iconOnly
                  icon={<ChevronRight aria-hidden="true" />}
                  title={t.history.next}
                  aria-label={t.history.next}
                  disabled={!hasOlder(offset, total)}
                  onClick={() => setOffset(offset + PAGE_SIZE)}
                />
              </span>
            </div>
          </Card>
          <Inspector row={activeRow} onRetried={onRetried} onDelete={onDelete} />
        </div>
      )}

      <ConfirmSheet
        open={confirmClear}
        title={t.history.clearTitle}
        body={t.history.clearBody}
        confirmLabel={t.history.clearConfirm}
        destructive
        onConfirm={clearAll}
        onCancel={() => setConfirmClear(false)}
      />
    </>
  );
}
