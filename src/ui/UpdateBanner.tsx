import { ArrowDownToLine, LoaderCircle, X } from "lucide-react";
import { useNavigation } from "../app/navigation";
import { useUpdates } from "../app/updates";
import { useT } from "../i18n";
import { Button } from "./Button";

/** A quiet strip at the top of the content column while a newer version waits. */
export function UpdateBanner() {
  const t = useT();
  const { state, install, dismissed, dismiss } = useUpdates();
  const { navigate } = useNavigation();

  const info =
    state.kind === "available" || state.kind === "installing"
      ? state.info
      : state.kind === "error" && state.info
        ? state.info
        : null;
  if (!info || dismissed) {
    return null;
  }
  const installing = state.kind === "installing";
  const failed = state.kind === "error";

  return (
    <div className="banner" role="status">
      {installing ? (
        <LoaderCircle className="spin" aria-hidden="true" />
      ) : (
        <ArrowDownToLine aria-hidden="true" />
      )}
      <div className="banner__text">
        <span className="banner__title">{t.settings.updateAvailable(info.version)}</span>
        {failed && <span className="banner__error">{t.settings.updateInstallFailed}</span>}
      </div>
      <div className="banner__actions">
        <Button size="sm" variant="quiet" onClick={() => navigate("settings", "about")}>
          {t.settings.releaseNotes}
        </Button>
        <Button size="sm" variant="primary" disabled={installing} onClick={install}>
          {installing ? t.settings.installing : t.settings.installAndRestart}
        </Button>
        <Button
          size="sm"
          variant="quiet"
          iconOnly
          icon={<X aria-hidden="true" />}
          title={t.settings.dismiss}
          aria-label={t.settings.dismiss}
          disabled={installing}
          onClick={dismiss}
        />
      </div>
    </div>
  );
}
