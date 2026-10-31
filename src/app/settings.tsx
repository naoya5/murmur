// The settings store of the main window: one copy of `Settings`, saved through `updateSettings`,
// refreshed by `settingsChanged`. Views read it with `useSettings()`.
import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { type ApplyError, commands, type Settings, useEvent } from "../ipc";

export type SettingsStore = {
  settings: Settings | null;
  /** Save a whole settings object. Resolves with the `ApplyError`, or null on success. */
  save: (next: Settings) => Promise<ApplyError | null>;
  /** Save a partial change on top of the current settings. */
  update: (patch: Partial<Settings>) => Promise<ApplyError | null>;
};

const SettingsContext = createContext<SettingsStore>({
  settings: null,
  save: async () => null,
  update: async () => null,
});

export function useSettings(): SettingsStore {
  return useContext(SettingsContext);
}

export function SettingsProvider({ children }: { children: ReactNode }) {
  const [settings, setSettingsState] = useState<Settings | null>(null);
  // The latest value, for patches made before a re-render (two quick toggles).
  const latest = useRef<Settings | null>(null);
  const setSettings = useCallback((next: Settings) => {
    latest.current = next;
    setSettingsState(next);
  }, []);

  useEffect(() => {
    commands
      .getSettings()
      .then(setSettings)
      .catch(() => undefined);
  }, [setSettings]);
  useEvent("settingsChanged", (payload) => setSettings(payload.settings));

  const save = useCallback(
    async (next: Settings): Promise<ApplyError | null> => {
      const result = await commands.updateSettings(next);
      if (result.status === "ok") {
        setSettings(result.data);
        return null;
      }
      return result.error;
    },
    [setSettings],
  );

  const update = useCallback(
    async (patch: Partial<Settings>): Promise<ApplyError | null> => {
      const current = latest.current ?? (await commands.getSettings());
      const next = { ...current, ...patch };
      latest.current = next;
      return save(next);
    },
    [save],
  );

  const value = useMemo(() => ({ settings, save, update }), [settings, save, update]);
  return <SettingsContext.Provider value={value}>{children}</SettingsContext.Provider>;
}
