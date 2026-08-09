import type { ReactNode } from "react";
import { useIsMobile } from "../lib/useIsMobile";

/** In-column page heading rendered only at the mobile breakpoint. */
export function MobileTitle({ children, className }: { children: ReactNode; className?: string }) {
  const isMobile = useIsMobile();
  if (!isMobile) return null;
  return (
    <div
      className={`text-[21px] leading-[1.15] font-medium tracking-[-0.025em] text-ink${className ? ` ${className}` : ""}`}
    >
      {children}
    </div>
  );
}
