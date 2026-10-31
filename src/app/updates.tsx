// Update state shared by the About section and the banner: one check, one install, one truth.
import { createContext, type ReactNode, useCallback, useContext, useMemo, useState } from "react";
import { commands, type UpdateError, type UpdateInfo, useEvent } from "../ipc";

export type UpdateState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "up_to_date" }
  | { kind: "available"; info: UpdateInfo }
  | { kind: "installing"; info: UpdateInfo }
  | { kind: "error"; error: UpdateError; info: UpdateInfo | null };

export type UpdateStore = {
  state: UpdateState;
  /** Ask the update endpoint now (the About button). */
  check: () => Promise<void>;
  /** Download, verify, install and restart. The app quits on success. */
  install: () => Promise<void>;
  /** The banner was dismissed for this session. */
  dismissed: boolean;
  dismiss: () => void;
};

const UpdateContext = createContext<UpdateStore>({
  state: { kind: "idle" },
  check: async () => {},
  install: async () => {},
  dismissed: false,
  dismiss: () => {},
});

export function useUpdates(): UpdateStore {
  return useContext(UpdateContext);
}

export function UpdateProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<UpdateState>({ kind: "idle" });
  const [dismissed, setDismissed] = useState(false);

  useEvent("updateAvailable", ({ info }) => {
    setState((current) => (current.kind === "installing" ? current : { kind: "available", info }));
  });

  const check = useCallback(async () => {
    setState({ kind: "checking" });
    const result = await commands.checkForUpdates();
    if (result.status === "error") {
      setState({ kind: "error", error: result.error, info: null });
      return;
    }
    if (result.data) {
      setState({ kind: "available", info: result.data });
      setDismissed(false);
    } else {
      setState({ kind: "up_to_date" });
    }
  }, []);

  const install = useCallback(async () => {
    const info = state.kind === "available" || state.kind === "error" ? state.info : null;
    if (!info) {
      return;
    }
    setState({ kind: "installing", info });
    const result = await commands.installUpdate();
    if (result.status === "error") {
      setState({ kind: "error", error: result.error, info });
    }
    // On success the app restarts; nothing to show.
  }, [state]);

  const value = useMemo(
    () => ({ state, check, install, dismissed, dismiss: () => setDismissed(true) }),
    [state, check, install, dismissed],
  );
  return <UpdateContext.Provider value={value}>{children}</UpdateContext.Provider>;
}
