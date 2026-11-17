import { ChevronRight } from "lucide-react";
import type { HTMLAttributes, ReactNode } from "react";

type CardProps = HTMLAttributes<HTMLDivElement> & { tight?: boolean };

export function Card({ tight, className, children, ...rest }: CardProps) {
  const cls = ["card", tight ? "card--tight" : "", className ?? ""].filter(Boolean).join(" ");
  return (
    <div className={cls} {...rest}>
      {children}
    </div>
  );
}

type GroupProps = { title?: string; badge?: ReactNode; children: ReactNode; id?: string };

export function Group({ title, badge, children, id }: GroupProps) {
  return (
    <section className="settings__section" id={id}>
      {title && (
        <h2 className={badge ? "group__title group__title--with-badge" : "group__title"}>
          {title}
          {badge}
        </h2>
      )}
      <div className="group">{children}</div>
    </section>
  );
}

type RowProps = {
  icon?: ReactNode;
  label: ReactNode;
  hint?: ReactNode;
  error?: string | null;
  children?: ReactNode;
  /** Stack the control under the label (for wide controls). */
  stack?: boolean;
};

export function Row({ icon, label, hint, error, children, stack }: RowProps) {
  return (
    <div className={stack ? "row row--stack" : "row"}>
      {icon && (
        <span className="row__icon" aria-hidden="true">
          {icon}
        </span>
      )}
      <div className="row__body">
        <span className="row__label">{label}</span>
        {hint && <span className="row__hint">{hint}</span>}
        {error && (
          <span className="row__error" role="alert">
            {error}
          </span>
        )}
      </div>
      {children && <div className="row__control">{children}</div>}
    </div>
  );
}

type LinkRowProps = {
  icon?: ReactNode;
  label: ReactNode;
  hint?: ReactNode;
  onClick: () => void;
  trailing?: ReactNode;
};

export function LinkRow({ icon, label, hint, onClick, trailing }: LinkRowProps) {
  return (
    <button type="button" className="row row--link" onClick={onClick}>
      {icon && (
        <span className="row__icon" aria-hidden="true">
          {icon}
        </span>
      )}
      <div className="row__body">
        <span className="row__label">{label}</span>
        {hint && <span className="row__hint">{hint}</span>}
      </div>
      <div className="row__control row__control--link">
        {trailing}
        <ChevronRight className="row__chevron" aria-hidden="true" />
      </div>
    </button>
  );
}
