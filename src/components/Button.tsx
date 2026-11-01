import type { ButtonHTMLAttributes, ReactNode, Ref } from "react";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & {
  ref?: Ref<HTMLButtonElement>;
  variant?: "default" | "primary" | "quiet" | "danger" | "danger-primary";
  size?: "sm" | "md" | "lg";
  icon?: ReactNode;
  /** Icon-only button; `title` or `aria-label` must describe it. */
  iconOnly?: boolean;
};

export function Button({
  variant = "default",
  size = "md",
  icon,
  iconOnly = false,
  className,
  children,
  type = "button",
  ...rest
}: Props) {
  const classes = ["btn"];
  if (variant === "primary") {
    classes.push("btn--primary");
  } else if (variant === "quiet") {
    classes.push("btn--quiet");
  } else if (variant === "danger") {
    classes.push("btn--danger");
  } else if (variant === "danger-primary") {
    classes.push("btn--danger", "btn--primary");
  }
  if (size === "sm") {
    classes.push("btn--sm");
  } else if (size === "lg") {
    classes.push("btn--lg");
  }
  if (iconOnly) {
    classes.push("btn--icon");
  }
  if (className) {
    classes.push(className);
  }
  return (
    <button type={type} className={classes.join(" ")} {...rest}>
      {icon}
      {!iconOnly && children}
    </button>
  );
}
