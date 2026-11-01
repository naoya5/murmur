import { BookA, Clock, House, Layers, Settings as SettingsIcon } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";
import {
  NavigationContext,
  parseTarget,
  type Target,
  useNavigation,
  type View,
} from "./app/navigation";
import { SettingsProvider, useSettings } from "./app/settings";
import { UpdateProvider } from "./app/updates";
import { I18nContext, resolveLocale, strings, useT } from "./i18n";
import { useEvent } from "./ipc";
import { ToastProvider } from "./ui/Toast";
import { UpdateBanner } from "./ui/UpdateBanner";
import { Dictionary } from "./views/Dictionary";
import { History } from "./views/History";
import { Home } from "./views/Home";
import { Models } from "./views/Models";
import { Onboarding } from "./views/Onboarding";
import { Settings } from "./views/Settings";

function Localized({ children }: { children: ReactNode }) {
  const { settings } = useSettings();
  const locale = resolveLocale(settings?.ui_language ?? "system");
  useEffect(() => {
    document.documentElement.lang = locale;
  }, [locale]);
  return <I18nContext.Provider value={strings[locale]}>{children}</I18nContext.Provider>;
}

function Sidebar() {
  const t = useT();
  const { view, navigate } = useNavigation();
  const items: { id: View; label: string; icon: ReactNode }[] = [
    { id: "home", label: t.nav.home, icon: <House aria-hidden="true" /> },
    { id: "history", label: t.nav.history, icon: <Clock aria-hidden="true" /> },
    { id: "dictionary", label: t.nav.dictionary, icon: <BookA aria-hidden="true" /> },
    { id: "models", label: t.nav.models, icon: <Layers aria-hidden="true" /> },
    { id: "settings", label: t.nav.settings, icon: <SettingsIcon aria-hidden="true" /> },
  ];
  return (
    <nav className="sidebar" aria-label={t.app.name}>
      {items.map((item) => (
        <button
          key={item.id}
          type="button"
          className="sidebar__item"
          aria-current={view === item.id ? "page" : undefined}
          onClick={() => navigate(item.id)}
        >
          {item.icon}
          {item.label}
        </button>
      ))}
    </nav>
  );
}

function CurrentView({ view }: { view: View }) {
  switch (view) {
    case "home":
      return <Home />;
    case "history":
      return <History />;
    case "dictionary":
      return <Dictionary />;
    case "models":
      return <Models />;
    case "settings":
      return <Settings />;
  }
}

function Shell() {
  const { settings } = useSettings();
  const [target, setTarget] = useState<Target>({ view: "home", anchor: null });
  const view = target.view;
  useEvent("navigate", (payload) => setTarget(parseTarget(payload.view)));
  const nav = useMemo(
    () => ({
      view,
      target,
      navigate: (next: View, anchor?: string) => setTarget({ view: next, anchor: anchor ?? null }),
    }),
    [view, target],
  );

  if (!settings) {
    return <div className="titlebar" data-tauri-drag-region />;
  }

  if (!settings.onboarding_completed) {
    return (
      <>
        <div className="titlebar" data-tauri-drag-region />
        <Onboarding />
      </>
    );
  }

  return (
    <NavigationContext.Provider value={nav}>
      <div className="shell">
        <div className="titlebar" data-tauri-drag-region />
        <Sidebar />
        <main className="content">
          <div className="content__rim" aria-hidden="true" />
          <div className="content__drag" data-tauri-drag-region />
          <UpdateBanner />
          <div className="view" key={view}>
            <div className="view__inner">
              <CurrentView view={view} />
            </div>
          </div>
        </main>
      </div>
    </NavigationContext.Provider>
  );
}

export default function App() {
  return (
    <SettingsProvider>
      <Localized>
        <ToastProvider>
          <UpdateProvider>
            <Shell />
          </UpdateProvider>
        </ToastProvider>
      </Localized>
    </SettingsProvider>
  );
}
