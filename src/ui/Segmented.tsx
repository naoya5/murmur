import { useId, useLayoutEffect, useRef, useState } from "react";

export type SegmentOption<V extends string> = { value: V; label: string };

type Props<V extends string> = {
  value: V;
  options: SegmentOption<V>[];
  onChange: (value: V) => void;
  label: string;
  disabled?: boolean;
};

/**
 * Native radios under a segmented skin, so keyboard and screen readers get the real thing.
 * A raised thumb slides under the selected label (the transition is off under reduced motion
 * through the duration tokens).
 */
export function Segmented<V extends string>({
  value,
  options,
  onChange,
  label,
  disabled,
}: Props<V>) {
  const name = useId();
  const hostRef = useRef<HTMLFieldSetElement>(null);
  const [thumb, setThumb] = useState<{ left: number; width: number } | null>(null);

  useLayoutEffect(() => {
    const host = hostRef.current;
    if (!host) {
      return;
    }
    const measure = () => {
      const index = options.findIndex((option) => option.value === value);
      const items = host.querySelectorAll<HTMLElement>(".segmented__item");
      const checked = index >= 0 ? items[index] : undefined;
      if (!checked) {
        setThumb(null);
        return;
      }
      setThumb({ left: checked.offsetLeft, width: checked.offsetWidth });
    };
    measure();
    if (typeof ResizeObserver === "undefined") {
      return;
    }
    const observer = new ResizeObserver(measure);
    observer.observe(host);
    return () => observer.disconnect();
  }, [value, options]);

  return (
    <fieldset ref={hostRef} className="segmented" aria-label={label} disabled={disabled}>
      {thumb && (
        <span
          className="segmented__thumb"
          aria-hidden="true"
          style={{ transform: `translateX(${thumb.left}px)`, width: thumb.width }}
        />
      )}
      {options.map((option) => (
        <label key={option.value} className="segmented__item">
          <input
            type="radio"
            className="sr-only"
            name={name}
            value={option.value}
            checked={option.value === value}
            onChange={() => onChange(option.value)}
          />
          {option.label}
        </label>
      ))}
    </fieldset>
  );
}
