import { createContext, useContext } from "react";

export const VIEWS = ["home", "history", "dictionary", "models", "settings"] as const;
export type View = (typeof VIEWS)[number];

export function asView(name: string): View {
  return (VIEWS as readonly string[]).includes(name) ? (name as View) : "home";
}

export const NavigationContext = createContext<{ view: View; navigate: (view: View) => void }>({
  view: "home",
  navigate: () => {},
});

export function useNavigation() {
  return useContext(NavigationContext);
}
