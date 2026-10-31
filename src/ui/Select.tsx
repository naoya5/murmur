import { ChevronDown } from "lucide-react";

export type SelectOption<V extends string> = { value: V; label: string };

type Props<V extends string> = {
  value: V;
  options: SelectOption<V>[];
  onChange: (value: V) => void;
  label: string;
  disabled?: boolean;
};

export function Select<V extends string>({ value, options, onChange, label, disabled }: Props<V>) {
  return (
    <span className="select">
      <select
        aria-label={label}
        value={value}
        disabled={disabled}
        onChange={(e) => onChange(e.target.value as V)}
      >
        {options.map((option) => (
          <option key={option.value} value={option.value}>
            {option.label}
          </option>
        ))}
      </select>
      <ChevronDown aria-hidden="true" />
    </span>
  );
}
