import { Check, Download, Keyboard, Mic, MousePointerClick } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { useSettings } from "../app/settings";
import { languageName, useT } from "../i18n";
import { commands, type Language, type ModelInfo, type Permissions, useEvent } from "../ipc";
import { formatBytes, formatPercent } from "../lib/format";
import { displayAccelerator } from "../lib/hotkey";
import { Button } from "../ui/Button";
import { Card } from "../ui/Glass";
import { Kbd } from "../ui/KeyRecorder";
import { ProgressBar } from "../ui/ProgressBar";
import { Segmented } from "../ui/Segmented";
import { TextArea } from "../ui/TextField";
import { downloadErrorText } from "./Models";

const VAD_ID = "silero-vad-v6.2";
const STEPS = 4;

function Steps({ current }: { current: number }) {
  return (
    <div className="onboard__steps" aria-hidden="true">
      {Array.from({ length: STEPS }, (_, i) => (
        <span
          key={i.toString()}
          className={`onboard__step${i < current ? " onboard__step--done" : i === current ? " onboard__step--current" : ""}`}
        />
      ))}
    </div>
  );
}

export function Onboarding() {
  const t = useT();
  const { settings, update } = useSettings();
  const [step, setStep] = useState(0);
  const [permissions, setPermissions] = useState<Permissions | null>(null);
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [downloadError, setDownloadError] = useState<string | null>(null);
  const [testText, setTestText] = useState("");
  const [tested, setTested] = useState(false);

  const refreshPermissions = useCallback(() => {
    commands
      .getPermissions()
      .then(setPermissions)
      .catch(() => undefined);
  }, []);
  const loadModels = useCallback(() => {
    commands
      .listModels()
      .then(setModels)
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    refreshPermissions();
    loadModels();
  }, [refreshPermissions, loadModels]);

  // The accessibility grant happens in System Settings; poll while that step is shown.
  useEffect(() => {
    if (step !== 1) {
      return;
    }
    const id = window.setInterval(refreshPermissions, 1000);
    return () => window.clearInterval(id);
  }, [step, refreshPermissions]);

  useEvent("modelsChanged", loadModels);
  useEvent("modelProgress", ({ id, progress }) => {
    setModels((list) =>
      list.map((m) =>
        m.entry.id === id
          ? { ...m, status: { ...m.status, downloading: progress, error: null } }
          : m,
      ),
    );
  });

  if (!settings) {
    return null;
  }

  const language = settings.language;
  const hotkey = displayAccelerator(settings.hotkey);
  const needed = [settings.models[language], VAD_ID]
    .map((id) => models.find((m) => m.entry.id === id))
    .filter((m): m is ModelInfo => m !== undefined);
  const allInstalled = needed.length === 2 && needed.every((m) => m.status.installed);
  const anyDownloading = needed.some((m) => m.status.downloading !== null);

  const downloadAll = async () => {
    setDownloadError(null);
    for (const m of needed) {
      if (!m.status.installed && !m.status.downloading) {
        const result = await commands.downloadModel(m.entry.id);
        if (result.status === "error") {
          setDownloadError(result.error.message);
        }
      }
    }
    loadModels();
  };

  const finish = () => update({ onboarding_completed: true });

  const next = () => setStep((s) => Math.min(STEPS - 1, s + 1));
  const back = () => setStep((s) => Math.max(0, s - 1));

  const micGranted = permissions?.microphone === "Granted";
  const micDenied = permissions?.microphone === "Denied";

  return (
    <div className="onboard">
      <div className="onboard__panel">
        <Steps current={step} />
        <div className="onboard__body">
          {step === 0 && (
            <>
              <div className="onboard__icon">
                <Mic aria-hidden="true" />
              </div>
              <h1 className="onboard__title">{t.onboarding.welcome}</h1>
              <p className="onboard__text">{t.onboarding.welcomeText}</p>
              <p className="onboard__text">{t.onboarding.stepMicText}</p>
              <div className="onboard__status" aria-live="polite">
                {micGranted ? (
                  <span className="onboard__status onboard__status--ok">
                    <Check aria-hidden="true" />
                    {t.onboarding.micGranted}
                  </span>
                ) : micDenied ? (
                  <span>{t.onboarding.micDenied}</span>
                ) : null}
              </div>
              {!micGranted && !micDenied && (
                <Button
                  variant="primary"
                  size="lg"
                  onClick={() =>
                    commands
                      .requestMicrophone()
                      .then(refreshPermissions)
                      .catch(() => undefined)
                  }
                >
                  {t.onboarding.allowMic}
                </Button>
              )}
              {micDenied && (
                <Button size="lg" onClick={() => commands.openPrivacyPane("Microphone")}>
                  {t.settings.openSystemSettings}
                </Button>
              )}
            </>
          )}

          {step === 1 && (
            <>
              <div className="onboard__icon">
                <MousePointerClick aria-hidden="true" />
              </div>
              <h1 className="onboard__title">{t.onboarding.stepAx}</h1>
              <p className="onboard__text">{t.onboarding.stepAxText}</p>
              <div className="onboard__status" aria-live="polite">
                {permissions?.accessibility ? (
                  <span className="onboard__status onboard__status--ok">
                    <Check aria-hidden="true" />
                    {t.onboarding.axGranted}
                  </span>
                ) : (
                  <span>{t.onboarding.axPending}</span>
                )}
              </div>
              {!permissions?.accessibility && (
                <Button
                  variant="primary"
                  size="lg"
                  onClick={() =>
                    commands
                      .requestAccessibility()
                      .then(refreshPermissions)
                      .catch(() => undefined)
                  }
                >
                  {t.onboarding.allowAx}
                </Button>
              )}
            </>
          )}

          {step === 2 && (
            <>
              <div className="onboard__icon">
                <Download aria-hidden="true" />
              </div>
              <h1 className="onboard__title">{t.onboarding.stepModel}</h1>
              <p className="onboard__text">
                {t.onboarding.stepModelText(languageName(t, language))}
              </p>
              <div className="perm">
                <span className="muted">{t.onboarding.chooseLanguage}</span>
                <Segmented<Language>
                  label={t.onboarding.chooseLanguage}
                  value={language}
                  disabled={anyDownloading}
                  onChange={(lang) => update({ language: lang })}
                  options={[
                    { value: "ja", label: t.common.japanese },
                    { value: "en", label: t.common.english },
                  ]}
                />
              </div>
              <Card className="onboard__card" tight>
                {needed.map((m) => {
                  const p = m.status.downloading;
                  return (
                    <div key={m.entry.id} className="row">
                      <div className="row__body">
                        <span className="row__label">{m.entry.display_name}</span>
                        <span className="row__hint tnum">
                          {formatBytes(m.entry.bytes, t.locale)}
                          {p
                            ? `, ${formatPercent(p.received / Math.max(1, p.total), t.locale)}`
                            : ""}
                        </span>
                        {p && (
                          <div style={{ marginTop: 6 }}>
                            <ProgressBar
                              value={p.total > 0 ? p.received / p.total : undefined}
                              label={m.entry.display_name}
                            />
                          </div>
                        )}
                        {m.status.error && (
                          <span className="row__error" role="alert">
                            {downloadErrorText(m.status.error, t)}
                          </span>
                        )}
                      </div>
                      <div className="row__control">
                        {m.status.installed && (
                          <span className="onboard__status onboard__status--ok">
                            <Check aria-hidden="true" />
                          </span>
                        )}
                      </div>
                    </div>
                  );
                })}
              </Card>
              {downloadError && (
                <p className="row__error" role="alert">
                  {downloadErrorText(downloadError, t)}
                </p>
              )}
              {allInstalled ? (
                <span className="onboard__status onboard__status--ok">
                  <Check aria-hidden="true" />
                  {t.onboarding.modelReady}
                </span>
              ) : (
                <Button variant="primary" size="lg" disabled={anyDownloading} onClick={downloadAll}>
                  {anyDownloading ? t.models.downloading : t.onboarding.downloadAll}
                </Button>
              )}
            </>
          )}

          {step === 3 && (
            <>
              <div className="onboard__icon">
                <Keyboard aria-hidden="true" />
              </div>
              <h1 className="onboard__title">{t.onboarding.stepTest}</h1>
              <p className="onboard__text">{t.onboarding.stepTestText(hotkey)}</p>
              {!permissions?.accessibility && (
                <p className="onboard__text small">{t.onboarding.testCopyOnly}</p>
              )}
              <div className="onboard__card">
                <TextArea
                  label={t.onboarding.stepTest}
                  placeholder={t.onboarding.testPlaceholder}
                  value={testText}
                  autoFocus
                  onChange={(e) => {
                    setTestText(e.target.value);
                    if (e.target.value.trim().length > 0) {
                      setTested(true);
                    }
                  }}
                />
              </div>
              <div className="perm">
                <Kbd tokens={hotkey.split(" ")} />
                <Button
                  size="sm"
                  variant="quiet"
                  onClick={() => commands.toggleRecording().catch(() => undefined)}
                >
                  {t.onboarding.startWithButton}
                </Button>
              </div>
              <div className="onboard__status" aria-live="polite">
                {tested && (
                  <span className="onboard__status onboard__status--ok">
                    <Check aria-hidden="true" />
                    {t.onboarding.testSuccess}
                  </span>
                )}
              </div>
            </>
          )}
        </div>

        <div className="onboard__footer">
          <span>
            {step > 0 && (
              <Button variant="quiet" onClick={back}>
                {t.common.back}
              </Button>
            )}
          </span>
          <span className="onboard__footer-end">
            {step === 1 && !permissions?.accessibility && (
              <Button variant="quiet" onClick={next}>
                {t.common.skip}
              </Button>
            )}
            {step === 2 && !allInstalled && (
              <Button variant="quiet" onClick={next}>
                {t.common.skip}
              </Button>
            )}
            {step < STEPS - 1 ? (
              <Button
                variant="primary"
                onClick={next}
                disabled={
                  (step === 0 && !micGranted) ||
                  (step === 1 && !permissions?.accessibility) ||
                  (step === 2 && !allInstalled)
                }
              >
                {t.common.next}
              </Button>
            ) : (
              <Button variant="primary" onClick={finish}>
                {t.onboarding.finish}
              </Button>
            )}
          </span>
        </div>
      </div>
    </div>
  );
}
