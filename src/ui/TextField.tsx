import type { InputHTMLAttributes, ReactNode, TextareaHTMLAttributes } from "react";

type InputProps = InputHTMLAttributes<HTMLInputElement> & {
  label: string;
  invalid?: boolean;
  leading?: ReactNode;
  unit?: string;
  block?: boolean;
  className?: string;
};

export function TextField({
  label,
  invalid,
  leading,
  unit,
  block,
  className,
  ...rest
}: InputProps) {
  const classes = ["field"];
  if (invalid) {
    classes.push("field--invalid");
  }
  if (block) {
    classes.push("field--block");
  }
  if (className) {
    classes.push(className);
  }
  return (
    <span className={classes.join(" ")}>
      {leading}
      <input aria-label={label} aria-invalid={invalid || undefined} {...rest} />
      {unit && <span className="field--unit">{unit}</span>}
    </span>
  );
}

type NumberProps = Omit<InputProps, "type" | "value" | "onChange" | "min" | "max"> & {
  value: number;
  min: number;
  max: number;
  onCommit: (value: number) => void;
};

/** A number field that clamps to [min, max] and commits on blur or Return. */
export function NumberField({ value, min, max, onCommit, ...rest }: NumberProps) {
  const commit = (raw: string) => {
    const n = Number.parseInt(raw, 10);
    if (Number.isNaN(n)) {
      return;
    }
    const clamped = Math.min(max, Math.max(min, n));
    if (clamped !== value) {
      onCommit(clamped);
    }
  };
  return (
    <TextField
      type="number"
      inputMode="numeric"
      min={min}
      max={max}
      key={value}
      defaultValue={value}
      onBlur={(e) => commit(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          commit((e.target as HTMLInputElement).value);
        }
      }}
      {...rest}
    />
  );
}

type AreaProps = TextareaHTMLAttributes<HTMLTextAreaElement> & { label: string };

export function TextArea({ label, className, ...rest }: AreaProps) {
  return (
    <span className={`field field--area field--block ${className ?? ""}`}>
      <textarea aria-label={label} {...rest} />
    </span>
  );
}
