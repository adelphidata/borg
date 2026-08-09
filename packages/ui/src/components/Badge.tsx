import type { ReactNode } from "react";

/** Square service-initials badge; neon border while the service is connected. */
export function Badge({
  on = true,
  sizeClasses = "h-[26px] w-[26px] rounded-lg text-[10px]",
  children,
}: {
  on?: boolean;
  /** Box size + corner radius + font-size, overridable for the list and detail variants. */
  sizeClasses?: string;
  children: ReactNode;
}) {
  return (
    <span
      className={`flex flex-none items-center justify-center border leading-none font-semibold tracking-[0.04em] ${sizeClasses} ${
        on ? "border-neon text-neon-ink" : "border-line text-ink3"
      }`}
    >
      {children}
    </span>
  );
}
