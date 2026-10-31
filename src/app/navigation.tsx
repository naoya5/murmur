import { createContext, useContext } from "react";

export const VIEWS = ["home", "history", "dictionary", "models", "settings"] as const;
export type View = (typeof VIEWS)[number];

export type Target = { view: View; anchor: string | null };

/** "settings#about" -> settings view, scrolled to the element with id "about". Unknown -> home. */
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
  navigate: (view: View, anchor?: string) => void;
}>({
  view: "home",
  navigate: () => {},
});

export function useNavigation() {
  return useContext(NavigationContext);
}
