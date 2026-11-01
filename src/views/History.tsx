import { ChevronLeft, ChevronRight, Clock, Copy, RotateCcw, Search, Trash2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { useT } from "../i18n";
import {
  commands,
  type FailureKind,
  type HistoryPage,
  type RetryError,
  type TranscriptRow,
  useEvent,
} from "../ipc";
import { formatWhen } from "../lib/format";
import { Badge, LangBadge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { EmptyState } from "../ui/EmptyState";
import { Card } from "../ui/Glass";
import { ConfirmSheet } from "../ui/Sheet";
import { TextField } from "../ui/TextField";
import { useToast } from "../ui/Toast";
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

function Item({ row, onRetried, onDelete }: ItemProps) {
  const t = useT();
  const toast = useToast();
  const [expanded, setExpanded] = useState(false);
  const [retrying, setRetrying] = useState(false);
  const [retryError, setRetryError] = useState<string | null>(null);

  const copy = async () => {
    if (!row.text) {
      return;
    }
    const result = await commands.copyText(row.text);
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

  return (
    <div className="hist__item">
      <div style={{ paddingTop: 1 }}>
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
      </div>
      <div style={{ minWidth: 0 }}>
        <div className="hist__meta">
          <span>{formatWhen(row.created_at_ms, t.locale)}</span>
          {failed && <Badge tone="warn">{t.history.statusFailed}</Badge>}
          {row.status === "recovered" && <Badge tone="ok">{t.history.statusRecovered}</Badge>}
          {retrying && <span>{t.history.retrying}</span>}
        </div>
        {failed ? (
          <p className="hist__text hist__text--error">{failureText(row.error, t)}</p>
        ) : (
          <p className={`hist__text selectable${clamp ? " hist__text--clamp" : ""}`}>{text}</p>
        )}
        {!failed && needsClamp(text) && (
          <button type="button" className="hist__expand" onClick={() => setExpanded((v) => !v)}>
            {expanded ? t.history.collapse : t.history.expand}
          </button>
        )}
        {retryError && (
          <p className="row__error" role="alert">
            {retryError}
          </p>
        )}
      </div>
      <div className="hist__actions">
        {failed && (
          <Button
            size="sm"
            icon={<RotateCcw aria-hidden="true" />}
            onClick={retry}
            disabled={retrying}
          >
            {t.common.retry}
          </Button>
        )}
        {!failed && (
          <Button
            size="sm"
            variant="quiet"
            iconOnly
            icon={<Copy aria-hidden="true" />}
            title={t.common.copy}
            aria-label={t.common.copy}
            onClick={copy}
          />
        )}
        <Button
          size="sm"
          variant="quiet"
          iconOnly
          icon={<Trash2 aria-hidden="true" />}
          title={t.common.delete}
          aria-label={t.common.delete}
          onClick={() => onDelete(row.id)}
        />
      </div>
    </div>
  );
}

export function History() {
  const t = useT();
  const toast = useToast();
  const [query, setQuery] = useState("");
  const [debounced, setDebounced] = useState("");
  const [offset, setOffset] = useState(0);
  const [page, setPage] = useState<HistoryPage | null>(null);
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

      {page && total > 0 && (
        <>
          <Card tight>
            <div className="hist">
              {page.items.map((row) => (
                <Item key={row.id} row={row} onRetried={onRetried} onDelete={onDelete} />
              ))}
            </div>
          </Card>
          <div className="pager">
            <span>{t.history.pageOf(range.from, range.to, total)}</span>
            <span className="pager__buttons">
              <Button
                size="sm"
                icon={<ChevronLeft aria-hidden="true" />}
                disabled={!hasNewer(offset)}
                onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
              >
                {t.history.prev}
              </Button>
              <Button
                size="sm"
                disabled={!hasOlder(offset, total)}
                onClick={() => setOffset(offset + PAGE_SIZE)}
              >
                {t.history.next}
                <ChevronRight aria-hidden="true" />
              </Button>
            </span>
          </div>
        </>
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
