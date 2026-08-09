import type { ButtonHTMLAttributes } from "react";

type PillVariant = "default" | "primary" | "danger";

const BASE =
  "cursor-pointer border bg-transparent font-medium leading-none transition-[border-color,color,background] duration-150";

const VARIANT: Record<PillVariant, string> = {
  default: "border-line text-ink2 hover:border-neon hover:text-ink",
  primary: "border-neon text-neon-ink shadow-(--plateGlow) hover:bg-nav-on-bg",
  danger: "border-alert text-alert hover:bg-alert-soft",
};

interface PillProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: PillVariant;
  /** 8px corner radius instead of the fully round pill. */
  square?: boolean;
  /** Padding + font-size, overridable so callers (onboarding CTA) can size up. */
  sizeClasses?: string;
}

export function Pill({
  variant = "default",
  square = false,
  sizeClasses = "px-4 py-2 text-[12.5px]",
  className,
  ...props
}: PillProps) {
  const classes = [BASE, VARIANT[variant], sizeClasses, square ? "rounded-lg" : "rounded-full"];
  if (className) classes.push(className);
  return <button type="button" className={classes.join(" ")} {...props} />;
}
