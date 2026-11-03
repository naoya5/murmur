import { Download, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useNavigation } from "../app/navigation";
import { useSettings } from "../app/settings";
import { Badge } from "../components/Badge";
import { Button } from "../components/Button";
import { Card, Group } from "../components/Glass";
import { ProgressBar } from "../components/ProgressBar";
import { ConfirmSheet } from "../components/Sheet";
import { useToast } from "../components/Toast";
import { languageName, type Strings, useT } from "../i18n";
import { commands, type Language, type ModelInfo, useEvent } from "../ipc";
import { formatBytes, formatPercent } from "../lib/format";

export function downloadErrorText(message: string, t: Strings): string {
  const m = message.toLowerCase();
  if (/offline|network|connection|dns|resolve|timed? ?out/.test(m)) {
    return t.models.errorOffline;
  }
  if (/disk|space|enospc/.test(m)) {
    return t.models.errorDisk;
  }
  return `${t.models.errorGeneric} ${message}`;
}

type RowProps = {
  info: ModelInfo;
  /** The language this section is about, or null for the helper section. */
  language: Language | null;
  onDownload: (id: string) => void;
  onCancel: (id: string) => void;
  onDelete: (info: ModelInfo) => void;
  onUse: (id: string, language: Language) => void;
  pendingError: string | null;
};

function ModelRow({
  info,
  language,
  onDownload,
  onCancel,
  onDelete,
  onUse,
  pendingError,
}: RowProps) {
  const t = useT();
  const { entry, status } = info;
  const inUse = info.selected_for.length > 0;
  const usedHere = language !== null && info.selected_for.includes(language);
  const progress = status.downloading;
  const error = pendingError ?? status.error;
  // A refine model is named by its task, not by a tier.
  const tier =
    entry.tier === "internal" || entry.role === "refinement" ? null : t.models.tier[entry.tier];

  return (
    <div className="model">
      <div className="model__head">
        <div style={{ minWidth: 0 }}>
          <div className="model__name">
            <span>{entry.display_name}</span>
            {tier && <Badge>{tier}</Badge>}
            {entry.tasks.map((task) => (
              <Badge key={task}>{t.models.task[task]}</Badge>
            ))}
            {inUse && entry.tier !== "internal" && (
              <Badge tone="accent">
                {info.selected_for.map((l) => t.models.usedFor(languageName(t, l))).join(", ")}
              </Badge>
            )}
          </div>
          <div className="model__meta">
            <span className="tnum">{formatBytes(entry.bytes, t.locale)}</span>
            <span>{entry.license}</span>
            <span>{status.installed ? t.models.installed : t.models.notInstalled}</span>
          </div>
          {entry.memory_bytes !== null && (
            <div className="model__meta">
              <span className="tnum">
                {t.models.memory(formatBytes(entry.memory_bytes, t.locale))}
              </span>
            </div>
          )}
        </div>
        <div className="model__actions">
          {progress ? (
            <Button size="sm" icon={<X aria-hidden="true" />} onClick={() => onCancel(entry.id)}>
              {t.models.cancel}
            </Button>
          ) : status.installed ? (
            <>
              {language && !usedHere && (
                <Button size="sm" onClick={() => onUse(entry.id, language)}>
                  {t.models.chooseFor(languageName(t, language))}
                </Button>
              )}
              {entry.tier !== "internal" && (
                <Button
                  size="sm"
                  variant="quiet"
                  iconOnly
                  icon={<Trash2 aria-hidden="true" />}
                  title={inUse ? t.models.deleteInUse : t.models.delete}
                  aria-label={`${t.models.delete}: ${entry.display_name}`}
                  disabled={inUse}
                  onClick={() => onDelete(info)}
                />
              )}
            </>
          ) : (
            <Button
              size="sm"
              variant="primary"
              icon={<Download aria-hidden="true" />}
              onClick={() => onDownload(entry.id)}
            >
              {t.models.download}
            </Button>
          )}
        </div>
      </div>
      {progress && (
        <div className="model__progress">
          <ProgressBar
            value={progress.total > 0 ? progress.received / progress.total : undefined}
            label={`${t.models.downloading}: ${entry.display_name}`}
          />
          <span>
            {progress.total > 0 ? formatPercent(progress.received / progress.total, t.locale) : ""}
          </span>
          <span>{t.models.rate(formatBytes(progress.bytes_per_sec, t.locale))}</span>
        </div>
      )}
      {error && (
        <p className="model__error" role="alert">
          {downloadErrorText(error, t)}
        </p>
      )}
      {entry.attribution && <p className="model__attr">{entry.attribution}</p>}
    </div>
  );
}

export function Models() {
  const t = useT();
  const toast = useToast();
  const { settings, update } = useSettings();
  const { target } = useNavigation();
  const [models, setModels] = useState<ModelInfo[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [pendingErrors, setPendingErrors] = useState<Record<string, string>>({});
  const [pendingDelete, setPendingDelete] = useState<ModelInfo | null>(null);

  const load = useCallback(() => {
    commands
      .listModels()
      .then((list) => {
        setModels(list);
        setLoadError(null);
      })
      .catch((e: unknown) => setLoadError(String(e)));
  }, []);

  useEffect(load, [load]);
  useEvent("modelsChanged", load);
  useEvent("settingsChanged", load);
  useEvent("modelProgress", ({ id, progress }) => {
    setModels((list) =>
      list
        ? list.map((m) =>
            m.entry.id === id
              ? { ...m, status: { ...m.status, downloading: progress, error: null } }
              : m,
          )
        : list,
    );
  });

  const download = async (id: string) => {
    setPendingErrors((e) => {
      const { [id]: _removed, ...rest } = e;
      return rest;
    });
    const result = await commands.downloadModel(id);
    if (result.status === "error") {
      setPendingErrors((e) => ({ ...e, [id]: result.error.message }));
    } else {
      load();
    }
  };

  const cancel = async (id: string) => {
    await commands.cancelDownload(id);
    load();
  };

  const remove = async () => {
    if (!pendingDelete) {
      return;
    }
    const id = pendingDelete.entry.id;
    setPendingDelete(null);
    const result = await commands.deleteModel(id);
    if (result.status === "ok") {
      toast(t.common.deleted);
      load();
    } else {
      setPendingErrors((e) => ({ ...e, [id]: result.error.message }));
    }
  };

  const use = async (id: string, language: Language) => {
    if (!settings) {
      return;
    }
    const error = await update({ models: { ...settings.models, [language]: id } });
    if (error) {
      setPendingErrors((e) => ({ ...e, [id]: error.message }));
    }
  };

  const section = (language: Language) =>
    (models ?? []).filter(
      (m) => m.entry.role === "recognition" && m.entry.languages.includes(language),
    );
  const helpers = (models ?? []).filter((m) => m.entry.role === "voice_detection");
  // The translation models (experimental): shown while translation is on, or while one is on
  // this Mac (to delete it after switching off).
  const refiners = (models ?? []).filter((m) => m.entry.role === "refinement");
  const showRefiners =
    refiners.length > 0 &&
    (settings?.translate_enabled === true ||
      refiners.some((m) => m.status.installed || m.status.downloading !== null));

  // "models#refine" (the translation settings) brings that section into view once it is there.
  // biome-ignore lint/correctness/useExhaustiveDependencies: models arriving is what makes the section exist
  useEffect(() => {
    if (target.anchor === "refine") {
      document.getElementById("models-refine")?.scrollIntoView({ block: "start" });
    }
  }, [target, models !== null]);

  const renderRows = (list: ModelInfo[], language: Language | null) =>
    list.map((info) => (
      <ModelRow
        key={`${language ?? "other"}-${info.entry.id}`}
        info={info}
        language={language}
        onDownload={download}
        onCancel={cancel}
        onDelete={setPendingDelete}
        onUse={use}
        pendingError={pendingErrors[info.entry.id] ?? null}
      />
    ));

  return (
    <>
      <header className="view__header">
        <h1 className="view__title">{t.models.title}</h1>
      </header>
      <p className="view__lead">{t.models.lead}</p>

      {loadError && (
        <Card>
          <p className="muted">{t.models.loadError}</p>
          <p className="caption faint selectable">{loadError}</p>
        </Card>
      )}

      {models && (
        <div className="settings">
          <Group title={t.models.forJapanese}>{renderRows(section("ja"), "ja")}</Group>
          <Group title={t.models.forEnglish}>{renderRows(section("en"), "en")}</Group>
          {showRefiners && (
            <Group title={t.models.refine} id="models-refine">
              <p className="row__hint model__hint">{t.models.refineHint}</p>
              {renderRows(refiners, null)}
            </Group>
          )}
          {helpers.length > 0 && (
            <Group title={t.models.helper}>
              <p className="row__hint model__hint">{t.models.helperHint}</p>
              {renderRows(helpers, null)}
            </Group>
          )}
        </div>
      )}

      <ConfirmSheet
        open={pendingDelete !== null}
        title={t.models.deleteTitle(pendingDelete?.entry.display_name ?? "")}
        body={t.models.deleteBody}
        confirmLabel={t.common.delete}
        destructive
        onConfirm={remove}
        onCancel={() => setPendingDelete(null)}
      />
    </>
  );
}
