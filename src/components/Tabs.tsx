import { type KeyboardEvent, useId, useLayoutEffect, useRef, useState } from "react";

export type TabOption<V extends string> = { value: V; label: string };

type Props<V extends string> = {
  value: V;
  options: TabOption<V>[];
  onChange: (value: V) => void;
  label: string;
  /** From `useTabsId`; links each tab to its panel. */
  idBase: string;
};

export function tabPanelId(base: string, value: string): string {
  return `${base}-panel-${value}`;
}

export function tabId(base: string, value: string): string {
  return `${base}-tab-${value}`;
}

/**
 * A tab bar with a sliding underline. Arrow keys, Home and End move between tabs (automatic
 * activation), and only the selected tab is in the tab order.
 */
export function Tabs<V extends string>({ value, options, onChange, label, idBase }: Props<V>) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [indicator, setIndicator] = useState<{ left: number; width: number } | null>(null);

  useLayoutEffect(() => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    const measure = () => {
      const index = options.findIndex((option) => option.value === value);
      const selected = host.querySelectorAll<HTMLElement>('[role="tab"]')[index];
      setIndicator(selected ? { left: selected.offsetLeft, width: selected.offsetWidth } : null);
    };
    measure();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(measure);
    observer.observe(host);
    return () => observer.disconnect();
  }, [value, options]);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const index = options.findIndex((option) => option.value === value);
    let next = index;
    if (e.key === "ArrowRight") {
      next = (index + 1) % options.length;
    } else if (e.key === "ArrowLeft") {
      next = (index - 1 + options.length) % options.length;
    } else if (e.key === "Home") {
      next = 0;
    } else if (e.key === "End") {
      next = options.length - 1;
    } else {
      return;
    }
    e.preventDefault();
    const option = options[next];
    if (option) {
      onChange(option.value);
      document.getElementById(tabId(idBase, option.value))?.focus();
    }
  };

  return (
    <div ref={hostRef} className="tabs" role="tablist" aria-label={label} onKeyDown={onKeyDown}>
      {options.map((option) => {
        const selected = option.value === value;
        return (
          <button
            key={option.value}
            id={tabId(idBase, option.value)}
            type="button"
            role="tab"
            className="tabs__item"
            aria-selected={selected}
            aria-controls={tabPanelId(idBase, option.value)}
            tabIndex={selected ? 0 : -1}
            onClick={() => onChange(option.value)}
          >
            {option.label}
          </button>
        );
      })}
      {indicator && (
        <span
          className="tabs__indicator"
          aria-hidden="true"
          style={{ transform: `translateX(${indicator.left}px)`, width: indicator.width }}
        />
      )}
    </div>
  );
}

export function useTabsId(): string {
  return useId().replace(/:/g, "");
}
