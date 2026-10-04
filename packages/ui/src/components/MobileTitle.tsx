import type { ReactNode } from "react";
import { useIsMobile } from "../lib/useIsMobile";

/**
 * In-column page title rendered only at the mobile breakpoint. Visual only:
 * PageChrome already gives the page its <h1>, so this one is aria-hidden
 * rather than a second heading that says the same thing.
 */
export function MobileTitle({ children, className }: { children: ReactNode; className?: string }) {
  const isMobile = useIsMobile();
  if (!isMobile) return null;
  return (
    <div
      aria-hidden="true"
      className={`text-[21px] leading-[1.15] font-medium tracking-[-0.025em] text-ink${className ? ` ${className}` : ""}`}
    >
      {children}
    </div>
  );
}
