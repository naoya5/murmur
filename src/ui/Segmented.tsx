import { useId } from "react";

export type SegmentOption<V extends string> = { value: V; label: string };

type Props<V extends string> = {
  value: V;
  options: SegmentOption<V>[];
  onChange: (value: V) => void;
  label: string;
  disabled?: boolean;
};

/** Native radios under a segmented skin, so keyboard and screen readers get the real thing. */
export function Segmented<V extends string>({
  value,
  options,
  onChange,
  label,
  disabled,
}: Props<V>) {
  const name = useId();
  return (
    <fieldset className="segmented" aria-label={label} disabled={disabled}>
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
