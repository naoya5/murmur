type Props = {
  /** 0..1, or undefined for an indeterminate bar. */
  value?: number;
  label: string;
};

export function ProgressBar({ value, label }: Props) {
  const indeterminate = value === undefined;
  const pct = indeterminate ? 0 : Math.min(100, Math.max(0, value * 100));
  return (
    <div
      className={`progress${indeterminate ? " progress--indeterminate" : ""}`}
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={indeterminate ? undefined : Math.round(pct)}
    >
      <div className="progress__bar" style={indeterminate ? undefined : { width: `${pct}%` }} />
    </div>
  );
}
