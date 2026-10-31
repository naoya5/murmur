// The only module views import for IPC. Views never call `invoke` directly (guardrail G4).
// Types and commands come from the generated `bindings.ts`; do not edit that file by hand.
import type { EventCallback, UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useRef } from "react";
import type { Result } from "./bindings";

export * from "./bindings";

/** Return the data of an `ok` result, or throw the error payload. */
export function unwrap<T, E>(result: Result<T, E>): T {
  if (result.status === "ok") {
    return result.data;
  }
  throw new Error(JSON.stringify(result.error));
}

/**
 * Subscribe to a typed event for the lifetime of the component.
 * Usage: `useEvent(events.someEvent.listen, (event) => ...)`.
 */
export function useEvent<T>(
  listen: (callback: EventCallback<T>) => Promise<UnlistenFn>,
  handler: EventCallback<T>,
): void {
  const handlerRef = useRef(handler);
  handlerRef.current = handler;

  useEffect(() => {
    let cancelled = false;
    let unlisten: UnlistenFn | undefined;
    listen((event) => handlerRef.current(event)).then((fn) => {
      if (cancelled) {
        fn();
      } else {
        unlisten = fn;
      }
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [listen]);
}
