import type { ButtonHTMLAttributes } from "react";

type PillVariant = "default" | "primary" | "danger";

interface PillProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: PillVariant;
  /** 8px corner radius instead of the fully round pill. */
  square?: boolean;
}

export function Pill({ variant = "default", square = false, className, ...props }: PillProps) {
  const classes = ["pill"];
  if (variant !== "default") classes.push(variant);
  if (square) classes.push("square");
  if (className) classes.push(className);
  return <button type="button" className={classes.join(" ")} {...props} />;
}
