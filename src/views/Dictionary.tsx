import { BookA, Pencil, Plus, Trash2, X } from "lucide-react";
import { useState } from "react";
import { useSettings } from "../app/settings";
import { Badge } from "../components/Badge";
import { Button } from "../components/Button";
import { EmptyState } from "../components/EmptyState";
import { Card } from "../components/Glass";
import { Segmented } from "../components/Segmented";
import { ConfirmSheet } from "../components/Sheet";
import { TextField } from "../components/TextField";
import { useToast } from "../components/Toast";
import { useT } from "../i18n";
import type { DictionaryEntry, DictionaryScope } from "../ipc";

function newId(): string {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(16)}-${Math.random().toString(16).slice(2)}`;
}

type Draft = { id: string | null; written: string; readings: string[]; scope: DictionaryScope };

const EMPTY_DRAFT: Draft = { id: null, written: "", readings: [], scope: "both" };

function ReadingsInput({
  readings,
  onChange,
  label,
  placeholder,
}: {
  readings: string[];
  onChange: (readings: string[]) => void;
  label: string;
  placeholder: string;
}) {
  const [text, setText] = useState("");
  const commit = () => {
    const value = text.trim();
    if (value && !readings.includes(value)) {
      onChange([...readings, value]);
    }
    setText("");
  };
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
      {readings.length > 0 && (
        <div className="dict__chips">
          {readings.map((r) => (
            <span key={r} className="chip">
              {r}
              <button
                type="button"
                className="chip__remove"
                aria-label={`${r}: ${label}`}
                onClick={() => onChange(readings.filter((x) => x !== r))}
              >
                <X aria-hidden="true" />
              </button>
            </span>
          ))}
        </div>
      )}
      <TextField
        label={label}
        block
        placeholder={placeholder}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.nativeEvent.isComposing) {
            e.preventDefault();
            commit();
          }
        }}
      />
    </div>
  );
}

function EntryForm({
  draft,
  onChange,
  onSave,
  onCancel,
  error,
}: {
  draft: Draft;
  onChange: (draft: Draft) => void;
  onSave: () => void;
  onCancel: () => void;
  error: string | null;
}) {
  const t = useT();
  return (
    <Card tight>
      <form
        className="dict__form"
        onSubmit={(e) => {
          e.preventDefault();
          onSave();
        }}
      >
        <div className="dict__form-row">
          <span className="dict__form-label">{t.dictionary.written}</span>
          <div>
            <TextField
              label={t.dictionary.written}
              block
              autoFocus
              placeholder={t.dictionary.writtenPlaceholder}
              value={draft.written}
              invalid={!!error}
              onChange={(e) => onChange({ ...draft, written: e.target.value })}
            />
            {error && (
              <p className="row__error" role="alert" style={{ marginTop: 4 }}>
                {error}
              </p>
            )}
          </div>
        </div>
        <div className="dict__form-row">
          <span className="dict__form-label">{t.dictionary.readings}</span>
          <div>
            <ReadingsInput
              readings={draft.readings}
              onChange={(readings) => onChange({ ...draft, readings })}
              label={t.dictionary.readings}
              placeholder={t.dictionary.readingsPlaceholder}
            />
            <p className="dict__form-hint">{t.dictionary.readingsHint}</p>
          </div>
        </div>
        <div className="dict__form-row">
          <span className="dict__form-label">{t.dictionary.scope}</span>
          <div>
            <Segmented<DictionaryScope>
              label={t.dictionary.scope}
              value={draft.scope}
              onChange={(scope) => onChange({ ...draft, scope })}
              options={[
                { value: "ja", label: t.common.japanese },
                { value: "en", label: t.common.english },
                { value: "both", label: t.common.both },
              ]}
            />
          </div>
        </div>
        <div className="sheet__actions">
          <Button onClick={onCancel}>{t.common.cancel}</Button>
          <Button variant="primary" type="submit">
            {t.common.save}
          </Button>
        </div>
      </form>
    </Card>
  );
}

export function Dictionary() {
  const t = useT();
  const toast = useToast();
  const { settings, update } = useSettings();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [pendingDelete, setPendingDelete] = useState<DictionaryEntry | null>(null);

  const entries = settings?.dictionary ?? [];

  const scopeLabel = (scope: DictionaryScope) =>
    scope === "ja" ? t.common.japanese : scope === "en" ? t.common.english : t.common.both;

  const save = async () => {
    if (!draft) {
      return;
    }
    const written = draft.written.trim();
    if (!written) {
      setFormError(t.dictionary.needWritten);
      return;
    }
    const entry: DictionaryEntry = {
      id: draft.id ?? newId(),
      written,
      readings: draft.readings,
      scope: draft.scope,
    };
    const next = draft.id
      ? entries.map((e) => (e.id === draft.id ? entry : e))
      : [...entries, entry];
    const error = await update({ dictionary: next });
    if (error) {
      setFormError(error.message);
      return;
    }
    setDraft(null);
    setFormError(null);
    toast(t.common.saved);
  };

  const remove = async () => {
    if (!pendingDelete) {
      return;
    }
    const id = pendingDelete.id;
    setPendingDelete(null);
    const error = await update({ dictionary: entries.filter((e) => e.id !== id) });
    if (!error) {
      toast(t.common.deleted);
    }
  };

  return (
    <>
      <header className="view__header">
        <div>
          <h1 className="view__title">{t.dictionary.title}</h1>
        </div>
        <div className="view__actions">
          <Button
            variant="primary"
            icon={<Plus aria-hidden="true" />}
            onClick={() => {
              setFormError(null);
              setDraft({ ...EMPTY_DRAFT });
            }}
            disabled={draft !== null}
          >
            {t.dictionary.addEntry}
          </Button>
        </div>
      </header>
      <p className="view__lead">{t.dictionary.explain}</p>

      {draft && (
        <EntryForm
          draft={draft}
          onChange={(d) => {
            setDraft(d);
            if (formError) {
              setFormError(null);
            }
          }}
          onSave={save}
          onCancel={() => {
            setDraft(null);
            setFormError(null);
          }}
          error={formError}
        />
      )}

      {entries.length === 0 && !draft ? (
        <Card>
          <EmptyState
            icon={<BookA />}
            title={t.dictionary.emptyTitle}
            body={t.dictionary.emptyBody}
          />
        </Card>
      ) : (
        entries.length > 0 && (
          <Card tight>
            {entries.map((entry) => (
              <div key={entry.id} className="row">
                <div className="row__body">
                  <span className="row__label dict__written">{entry.written}</span>
                  {entry.readings.length > 0 && (
                    <div className="dict__chips" style={{ marginTop: 3 }}>
                      {entry.readings.map((r) => (
                        <span key={r} className="chip">
                          {r}
                        </span>
                      ))}
                    </div>
                  )}
                </div>
                <div className="row__control">
                  <Badge>{scopeLabel(entry.scope)}</Badge>
                  <Button
                    size="sm"
                    variant="quiet"
                    iconOnly
                    icon={<Pencil aria-hidden="true" />}
                    title={t.common.edit}
                    aria-label={`${t.common.edit}: ${entry.written}`}
                    disabled={draft !== null}
                    onClick={() => {
                      setFormError(null);
                      setDraft({
                        id: entry.id,
                        written: entry.written,
                        readings: entry.readings,
                        scope: entry.scope,
                      });
                    }}
                  />
                  <Button
                    size="sm"
                    variant="quiet"
                    iconOnly
                    icon={<Trash2 aria-hidden="true" />}
                    title={t.common.delete}
                    aria-label={`${t.common.delete}: ${entry.written}`}
                    onClick={() => setPendingDelete(entry)}
                  />
                </div>
              </div>
            ))}
          </Card>
        )
      )}

      <ConfirmSheet
        open={pendingDelete !== null}
        title={t.dictionary.deleteTitle(pendingDelete?.written ?? "")}
        body={t.dictionary.deleteBody}
        confirmLabel={t.common.delete}
        destructive
        onConfirm={remove}
        onCancel={() => setPendingDelete(null)}
      />
    </>
  );
}
