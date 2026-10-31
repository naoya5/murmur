import { Mic } from "lucide-react";
import { type ReactNode, useCallback, useEffect, useState } from "react";
import { useSettings } from "../app/settings";
import { useT } from "../i18n";
import { commands, type Dashboard, useEvent } from "../ipc";
import { durationParts, formatDuration, formatNumber } from "../lib/format";
import { displayAccelerator } from "../lib/hotkey";
import { Bars, Heatmap, Sparkline } from "../ui/Charts";
import { EmptyState } from "../ui/EmptyState";
import { Card } from "../ui/Glass";

function Stat({
  label,
  value,
  unit,
  children,
}: {
  label: string;
  value: string;
  unit?: string;
  children?: ReactNode;
}) {
  return (
    <Card className="stat">
      <span className="stat__label">{label}</span>
      <span className="stat__value">
        {value}
        {unit && <span className="stat__unit">{unit}</span>}
      </span>
      {children}
    </Card>
  );
}

export function Home() {
  const t = useT();
  const { settings } = useSettings();
  const [data, setData] = useState<Dashboard | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    commands
      .getDashboard()
      .then((result) => {
        if (result.status === "ok") {
          setData(result.data);
          setError(null);
        } else {
          setError(result.error.message);
        }
      })
      .catch((e: unknown) => setError(String(e)));
  }, []);

  useEffect(load, [load]);
  useEvent("historyChanged", load);
  useEvent("settingsChanged", load);

  const hotkey = displayAccelerator(settings?.hotkey ?? "Alt+Space");

  if (error) {
    return (
      <>
        <header className="view__header">
          <h1 className="view__title">{t.home.title}</h1>
        </header>
        <Card>
          <p className="muted">{t.home.loadError}</p>
          <p className="caption faint selectable">{error}</p>
        </Card>
      </>
    );
  }

  if (!data) {
    return (
      <header className="view__header">
        <h1 className="view__title">{t.home.title}</h1>
      </header>
    );
  }

  if (data.total_sessions === 0) {
    return (
      <>
        <header className="view__header">
          <h1 className="view__title">{t.home.title}</h1>
        </header>
        <Card>
          <EmptyState icon={<Mic />} title={t.home.emptyTitle} body={t.home.emptyBody(hotkey)} />
        </Card>
      </>
    );
  }

  const saved = durationParts(data.saved_ms, t);
  const rateJa = data.speaking_cpm_ja;
  const rateEn = data.speaking_wpm_en;
  const sessionsByDay = data.last_14_days.map((d) => d.sessions);

  return (
    <>
      <header className="view__header">
        <h1 className="view__title">{t.home.title}</h1>
      </header>

      <Card className="hero" tight>
        <span className="hero__label">{t.home.timeSaved}</span>
        <span className="hero__value">
          {saved.map((part) => (
            <span
              key={part.unit}
              style={{ display: "inline-flex", alignItems: "baseline", gap: 4 }}
            >
              {part.value}
              <span className="hero__unit">{part.unit}</span>
            </span>
          ))}
        </span>
        <span className="hero__sub">
          {t.home.timeSavedSub(
            formatDuration(data.total_speech_ms, t),
            formatDuration(data.typing_ms, t),
          )}
        </span>
      </Card>

      <div className="stats">
        <Stat label={t.home.sessions} value={formatNumber(data.total_sessions, t.locale)}>
          <span className="caption muted">{t.home.today(data.today_sessions)}</span>
          <div className="stat__spark">
            <Sparkline values={sessionsByDay} label={t.home.last14} />
          </div>
        </Stat>
        <Stat label={t.home.words} value={formatNumber(data.total_latin_words, t.locale)} />
        <Stat label={t.home.characters} value={formatNumber(data.total_cjk_chars, t.locale)} />
        <Stat
          label={t.home.speakingRate}
          value={
            rateJa !== null
              ? formatNumber(rateJa, t.locale)
              : rateEn !== null
                ? formatNumber(rateEn, t.locale)
                : "-"
          }
          unit={rateJa !== null ? t.home.cpm : rateEn !== null ? t.home.wpm : undefined}
        >
          {rateJa !== null && rateEn !== null && (
            <span className="caption muted tnum">
              {formatNumber(rateEn, t.locale)} {t.home.wpm}
            </span>
          )}
        </Stat>
        <Stat
          label={t.home.latency}
          value={
            data.median_latency_ms === null ? "-" : formatNumber(data.median_latency_ms, t.locale)
          }
          unit={data.median_latency_ms === null ? undefined : t.units.ms}
        >
          <span className="caption muted">{t.home.latencyHint}</span>
        </Stat>
        <Stat
          label={t.home.streak}
          value={formatNumber(data.streak_days, t.locale)}
          unit={data.streak_days === 1 ? t.home.day : t.home.days}
        />
      </div>

      <Card>
        <div className="chart-card__head">
          <span className="chart-card__title">{t.home.activity}</span>
          <span className="chart-card__meta">{t.home.lastWeeks}</span>
        </div>
        <Heatmap
          days={data.heatmap}
          locale={t.locale}
          label={`${t.home.activity}, ${t.home.lastWeeks}`}
        />
      </Card>

      <Card>
        <div className="chart-card__head">
          <span className="chart-card__title">{t.home.last14}</span>
          <span className="chart-card__meta">{t.home.perDay}</span>
        </div>
        <Bars
          days={data.last_14_days}
          locale={t.locale}
          label={`${t.home.last14}, ${t.home.perDay}`}
        />
      </Card>
    </>
  );
}
