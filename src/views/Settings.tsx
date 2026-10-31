import { ExternalLink } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useNavigation } from "../app/navigation";
import { useSettings } from "../app/settings";
import { type Strings, useT } from "../i18n";
import {
  type AppInfo,
  type ApplyError,
  commands,
  type InputDevice,
  type Language,
  type ModelInfo,
  type Permissions,
  type RecordMode,
  type Settings as SettingsType,
  type UiLanguage,
} from "../ipc";
import { Dot } from "../ui/Badge";
import { Button } from "../ui/Button";
import { Group, LinkRow, Row } from "../ui/Glass";
import { KeyRecorder } from "../ui/KeyRecorder";
import { Segmented } from "../ui/Segmented";
import { Select } from "../ui/Select";
import { ConfirmSheet } from "../ui/Sheet";
import { NumberField } from "../ui/TextField";
import { useToast } from "../ui/Toast";
import { Toggle } from "../ui/Toggle";

type Field = keyof SettingsType | "permissions" | "stats";

function applyErrorText(error: ApplyError, t: Strings): string {
  const head = t.settings.applyError[error.kind];
  return error.message ? `${head}: ${error.message}` : head;
}

const SYSTEM_DEFAULT = "__system_default__";

export function Settings() {
  const t = useT();
  const toast = useToast();
  const { navigate } = useNavigation();
  const { settings, update } = useSettings();
  const [errors, setErrors] = useState<Partial<Record<Field, string>>>({});
  const [devices, setDevices] = useState<InputDevice[]>([]);
  const [permissions, setPermissions] = useState<Permissions | null>(null);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [confirmReset, setConfirmReset] = useState(false);

  const refreshPermissions = useCallback(() => {
    commands
      .getPermissions()
      .then(setPermissions)
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    commands
      .listInputDevices()
      .then(setDevices)
      .catch(() => undefined);
    commands
      .appInfo()
      .then(setInfo)
      .catch(() => undefined);
    commands
      .listModels()
      .then(setModels)
      .catch(() => undefined);
    refreshPermissions();
    const onFocus = () => refreshPermissions();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refreshPermissions]);

  const set = useCallback(
    async (field: keyof SettingsType, patch: Partial<SettingsType>) => {
      const error = await update(patch);
      setErrors((e) => {
        const next = { ...e };
        if (error) {
          next[field] = applyErrorText(error, t);
        } else {
          delete next[field];
        }
        return next;
      });
      return error;
    },
    [update, t],
  );

  if (!settings) {
    return null;
  }

  const modelName = (language: Language) =>
    models.find((m) => m.entry.id === settings.models[language])?.entry.display_name ??
    settings.models[language];

  const permissionLabel = (p: Permissions["microphone"]) =>
    p === "Granted"
      ? t.settings.granted
      : p === "Denied"
        ? t.settings.denied
        : t.settings.notDetermined;

  const resetStats = async () => {
    setConfirmReset(false);
    const result = await commands.resetStats();
    setErrors((e) => {
      const next = { ...e };
      if (result.status === "ok") {
        delete next.stats;
      } else {
        next.stats = result.error.message;
      }
      return next;
    });
    if (result.status === "ok") {
      toast(t.common.done);
    }
  };

  const retentionDays =
    settings.history_retention.kind === "days" ? settings.history_retention.days : 30;

  return (
    <>
      <header className="view__header">
        <h1 className="view__title">{t.settings.title}</h1>
      </header>

      <div className="settings">
        <Group title={t.settings.general}>
          <Row
            label={t.settings.uiLanguage}
            hint={t.settings.uiLanguageHint}
            error={errors.ui_language}
          >
            <Segmented<UiLanguage>
              label={t.settings.uiLanguage}
              value={settings.ui_language}
              onChange={(ui_language) => set("ui_language", { ui_language })}
              options={[
                { value: "system", label: t.common.system },
                { value: "en", label: "English" },
                { value: "ja", label: "日本語" },
              ]}
            />
          </Row>
          <Row label={t.settings.launchAtLogin} error={errors.launch_at_login}>
            <Toggle
              label={t.settings.launchAtLogin}
              checked={settings.launch_at_login}
              onChange={(launch_at_login) => set("launch_at_login", { launch_at_login })}
            />
          </Row>
          <Row
            label={t.settings.autoCheckUpdates}
            hint={t.settings.autoCheckUpdatesHint}
            error={errors.auto_check_updates}
          >
            <Toggle
              label={t.settings.autoCheckUpdates}
              checked={settings.auto_check_updates}
              onChange={(auto_check_updates) => set("auto_check_updates", { auto_check_updates })}
            />
          </Row>
          <Row label={t.settings.sounds} error={errors.sounds}>
            <Toggle
              label={t.settings.sounds}
              checked={settings.sounds}
              onChange={(sounds) => set("sounds", { sounds })}
            />
          </Row>
        </Group>

        <Group title={t.settings.recording}>
          <Row label={t.settings.hotkey} hint={t.settings.hotkeyHint} error={errors.hotkey}>
            <KeyRecorder
              label={t.settings.hotkey}
              value={settings.hotkey}
              onListenStart={() => commands.suspendHotkeys().catch(() => undefined)}
              onListenEnd={() => commands.resumeHotkeys().catch(() => undefined)}
              onChange={(hotkey) => {
                if (hotkey) {
                  set("hotkey", { hotkey });
                }
              }}
            />
          </Row>
          <Row
            label={t.settings.alternateHotkey}
            hint={t.settings.alternateHotkeyHint}
            error={errors.alternate_language_hotkey}
          >
            <KeyRecorder
              label={t.settings.alternateHotkey}
              value={settings.alternate_language_hotkey}
              clearable
              onListenStart={() => commands.suspendHotkeys().catch(() => undefined)}
              onListenEnd={() => commands.resumeHotkeys().catch(() => undefined)}
              onChange={(alternate_language_hotkey) =>
                set("alternate_language_hotkey", { alternate_language_hotkey })
              }
            />
          </Row>
          <Row label={t.settings.recordMode} error={errors.record_mode}>
            <Segmented<RecordMode>
              label={t.settings.recordMode}
              value={settings.record_mode}
              onChange={(record_mode) => set("record_mode", { record_mode })}
              options={[
                { value: "toggle", label: t.settings.toggle },
                { value: "push_to_talk", label: t.settings.pushToTalk },
              ]}
            />
          </Row>
          <Row label={t.settings.microphone} error={errors.microphone}>
            <Select
              label={t.settings.microphone}
              value={settings.microphone ?? SYSTEM_DEFAULT}
              onChange={(value) =>
                set("microphone", { microphone: value === SYSTEM_DEFAULT ? null : value })
              }
              options={[
                { value: SYSTEM_DEFAULT, label: t.settings.systemDefault },
                ...devices.map((d) => ({ value: d.name, label: d.name })),
                ...(settings.microphone && !devices.some((d) => d.name === settings.microphone)
                  ? [{ value: settings.microphone, label: settings.microphone }]
                  : []),
              ]}
            />
          </Row>
          <Row label={t.settings.language} hint={t.settings.languageHint} error={errors.language}>
            <Segmented<Language>
              label={t.settings.language}
              value={settings.language}
              onChange={(language) => set("language", { language })}
              options={[
                { value: "ja", label: t.common.japanese },
                { value: "en", label: t.common.english },
              ]}
            />
          </Row>
          <LinkRow
            label={t.settings.model}
            hint={`${t.common.japanese}: ${modelName("ja")}, ${t.common.english}: ${modelName("en")}`}
            onClick={() => navigate("models")}
            trailing={<span className="muted small">{t.settings.manageModels}</span>}
          />
          <Row
            label={t.settings.unloadModel}
            hint={t.settings.unloadModelHint}
            error={errors.unload_model_after_min}
          >
            <Select
              label={t.settings.unloadModel}
              value={
                settings.unload_model_after_min === null
                  ? "never"
                  : String(settings.unload_model_after_min)
              }
              onChange={(value) =>
                set("unload_model_after_min", {
                  unload_model_after_min: value === "never" ? null : Number(value),
                })
              }
              options={[
                { value: "never", label: t.settings.keepLoaded },
                ...[5, 10, 30, 60].map((min) => ({
                  value: String(min),
                  label: t.settings.unloadAfter(min),
                })),
                ...(settings.unload_model_after_min !== null &&
                ![5, 10, 30, 60].includes(settings.unload_model_after_min)
                  ? [
                      {
                        value: String(settings.unload_model_after_min),
                        label: t.settings.unloadAfter(settings.unload_model_after_min),
                      },
                    ]
                  : []),
              ]}
            />
          </Row>
        </Group>

        <Group title={t.settings.output}>
          <Row
            label={t.settings.autoPaste}
            hint={t.settings.autoPasteHint}
            error={errors.auto_paste}
          >
            <Toggle
              label={t.settings.autoPaste}
              checked={settings.auto_paste}
              onChange={(auto_paste) => set("auto_paste", { auto_paste })}
            />
          </Row>
          <Row
            label={t.settings.restoreClipboard}
            hint={t.settings.restoreClipboardHint}
            error={errors.restore_clipboard}
          >
            <Toggle
              label={t.settings.restoreClipboard}
              checked={settings.restore_clipboard}
              onChange={(restore_clipboard) => set("restore_clipboard", { restore_clipboard })}
            />
          </Row>
        </Group>

        <Group title={t.settings.text}>
          <Row
            label={t.settings.removeFillers}
            hint={t.settings.removeFillersHint}
            error={errors.remove_fillers}
          >
            <Toggle
              label={t.settings.removeFillers}
              checked={settings.remove_fillers}
              onChange={(remove_fillers) => set("remove_fillers", { remove_fillers })}
            />
          </Row>
          <Row label={t.settings.finalPunctuation} error={errors.add_final_punctuation}>
            <Toggle
              label={t.settings.finalPunctuation}
              checked={settings.add_final_punctuation}
              onChange={(add_final_punctuation) =>
                set("add_final_punctuation", { add_final_punctuation })
              }
            />
          </Row>
          <LinkRow
            label={t.settings.dictionary}
            hint={t.settings.dictionaryHint(settings.dictionary.length)}
            onClick={() => navigate("dictionary")}
            trailing={<span className="muted small">{t.settings.openDictionary}</span>}
          />
        </Group>

        <Group title={t.settings.stats}>
          <Row
            label={t.settings.typingWpm}
            hint={t.settings.typingHint}
            error={errors.typing_wpm_en}
          >
            <NumberField
              label={t.settings.typingWpm}
              value={settings.typing_wpm_en}
              min={5}
              max={300}
              unit={t.home.wpm}
              onCommit={(typing_wpm_en) => set("typing_wpm_en", { typing_wpm_en })}
            />
          </Row>
          <Row label={t.settings.typingCpm} error={errors.typing_cpm_ja}>
            <NumberField
              label={t.settings.typingCpm}
              value={settings.typing_cpm_ja}
              min={10}
              max={600}
              unit={t.home.cpm}
              onCommit={(typing_cpm_ja) => set("typing_cpm_ja", { typing_cpm_ja })}
            />
          </Row>
          <Row label={t.settings.resetStats} hint={t.settings.resetBody} error={errors.stats}>
            <Button variant="danger" onClick={() => setConfirmReset(true)}>
              {t.settings.resetStats}
            </Button>
          </Row>
        </Group>

        <Group title={t.settings.privacy}>
          <Row label={t.settings.retention} error={errors.history_retention}>
            {settings.history_retention.kind === "days" && (
              <NumberField
                label={t.settings.days}
                value={retentionDays}
                min={1}
                max={3650}
                unit={t.settings.days}
                onCommit={(days) =>
                  set("history_retention", { history_retention: { kind: "days", days } })
                }
              />
            )}
            <Segmented<"forever" | "days">
              label={t.settings.retention}
              value={settings.history_retention.kind}
              onChange={(kind) =>
                set("history_retention", {
                  history_retention: kind === "forever" ? { kind } : { kind, days: retentionDays },
                })
              }
              options={[
                { value: "forever", label: t.settings.forever },
                { value: "days", label: t.settings.forDays },
              ]}
            />
          </Row>
          <Row
            label={
              <span className="perm">
                <Dot
                  tone={
                    permissions?.microphone === "Granted"
                      ? "ok"
                      : permissions?.microphone === "Denied"
                        ? "bad"
                        : "none"
                  }
                />
                {t.settings.micPermission}
              </span>
            }
            hint={permissions ? permissionLabel(permissions.microphone) : undefined}
          >
            {permissions?.microphone === "NotDetermined" && (
              <Button
                size="sm"
                onClick={() =>
                  commands
                    .requestMicrophone()
                    .then(refreshPermissions)
                    .catch(() => undefined)
                }
              >
                {t.settings.allow}
              </Button>
            )}
            {permissions?.microphone === "Denied" && (
              <Button size="sm" onClick={() => commands.openPrivacyPane("Microphone")}>
                {t.settings.openSystemSettings}
              </Button>
            )}
          </Row>
          <Row
            label={
              <span className="perm">
                <Dot tone={permissions?.accessibility ? "ok" : "warn"} />
                {t.settings.accessibilityPermission}
              </span>
            }
            hint={t.settings.accessibilityHint}
          >
            {permissions?.accessibility && (
              <span className="muted small">{t.settings.granted}</span>
            )}
            {permissions && !permissions.accessibility && (
              <>
                <Button
                  size="sm"
                  onClick={() =>
                    commands
                      .requestAccessibility()
                      .then(refreshPermissions)
                      .catch(() => undefined)
                  }
                >
                  {t.settings.allow}
                </Button>
                <Button
                  size="sm"
                  variant="quiet"
                  onClick={() => commands.openPrivacyPane("Accessibility")}
                >
                  {t.settings.openSystemSettings}
                </Button>
              </>
            )}
          </Row>
          <Row
            label={t.settings.dataDir}
            hint={<span className="datadir selectable">{info?.data_dir ?? ""}</span>}
          />
          <Row label={t.settings.privacyNote} />
        </Group>

        <Group title={t.settings.about}>
          <Row label={t.settings.version} hint={t.app.name}>
            <span className="tnum">{info?.version ?? ""}</span>
          </Row>
          <Row label={t.settings.engine}>
            <span>{info?.speech_engine ?? ""}</span>
          </Row>
          <Row label={t.settings.schema}>
            <span className="tnum">{settings.schema_version}</span>
          </Row>
          <Row label={t.settings.licenses} stack>
            <div className="settings" style={{ gap: 10, width: "100%" }}>
              <div className="license">
                <span className="license__name">speech.cpp</span>
                <span className="license__text">MIT</span>
              </div>
              {models.map((m) => (
                <div key={m.entry.id} className="license">
                  <span className="license__name">{m.entry.display_name}</span>
                  <span className="license__text">
                    {m.entry.license}
                    {m.entry.attribution ? `. ${m.entry.attribution}` : ""}
                  </span>
                  <span>
                    <button
                      type="button"
                      className="btn btn--quiet btn--sm"
                      onClick={() =>
                        commands
                          .openUrl(`https://huggingface.co/${m.entry.repository}`)
                          .catch(() => undefined)
                      }
                    >
                      <ExternalLink aria-hidden="true" />
                      {m.entry.repository}
                    </button>
                  </span>
                </div>
              ))}
              <div className="license">
                <span className="license__name">lucide-react</span>
                <span className="license__text">ISC</span>
              </div>
            </div>
          </Row>
          <Row
            label={t.settings.showOnboarding}
            hint={settings.onboarding_completed ? t.settings.onboardingDone : undefined}
            error={errors.onboarding_completed}
          >
            <Button
              size="sm"
              onClick={() => set("onboarding_completed", { onboarding_completed: false })}
            >
              {t.settings.showOnboardingAction}
            </Button>
          </Row>
        </Group>
      </div>

      <ConfirmSheet
        open={confirmReset}
        title={t.settings.resetTitle}
        body={t.settings.resetBody}
        confirmLabel={t.settings.resetConfirm}
        destructive
        onConfirm={resetStats}
        onCancel={() => setConfirmReset(false)}
      />
    </>
  );
}
