// The only module views import for IPC. Views never call `invoke` directly (guardrail G4).
// Types and commands come from the generated `bindings.ts`; do not edit that file by hand.
import type { EventCallback, UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useRef } from "react";
import { events, type Result } from "./bindings";

export * from "./bindings";

/** Return the data of an `ok` result, or throw the error payload. */
export function unwrap<T, E>(result: Result<T, E>): T {
  if (result.status === "ok") {
    return result.data;
  }
  throw new Error(JSON.stringify(result.error));
}

export type EventName = keyof typeof events;

export type EventPayload<K extends EventName> = (typeof events)[K] extends {
  listen: (cb: EventCallback<infer P>) => unknown;
}
  ? P
  : never;

/** Subscribes for the lifetime of the component, once per event name; the handler may change freely. */
export function useEvent<K extends EventName>(
  name: K,
  handler: (payload: EventPayload<K>) => void,
): void {
  const handlerRef = useRef(handler);
  handlerRef.current = handler;

  useEffect(() => {
    let cancelled = false;
    let unlisten: UnlistenFn | undefined;
    const listen = events[name].listen as (
      cb: EventCallback<EventPayload<K>>,
    ) => Promise<UnlistenFn>;
    listen((event) => handlerRef.current(event.payload)).then((fn) => {
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
  }, [name]);
}
