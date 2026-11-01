import { createContext, useContext } from "react";

export const VIEWS = ["home", "history", "dictionary", "models", "settings"] as const;
export type View = (typeof VIEWS)[number];

export type Target = { view: View; anchor: string | null };

/** "settings#about" -> settings view with anchor "about" (the view decides what it opens). Unknown -> home. */
export function parseTarget(name: string): Target {
  const [head, anchor] = name.split("#", 2);
  const view = (VIEWS as readonly string[]).includes(head) ? (head as View) : "home";
  return { view, anchor: anchor || null };
}

export function asView(name: string): View {
  return parseTarget(name).view;
}

export const NavigationContext = createContext<{
  view: View;
  /** The last navigation, a new object each time, so a view can react to a repeated anchor. */
  target: Target;
  navigate: (view: View, anchor?: string) => void;
}>({
  view: "home",
  target: { view: "home", anchor: null },
  navigate: () => {},
});

export function useNavigation() {
  return useContext(NavigationContext);
}
