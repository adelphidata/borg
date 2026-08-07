import type { ReactNode } from "react";
import { useIsMobile } from "../lib/useIsMobile";

/** In-column page heading rendered only at the mobile breakpoint. */
export function MobileTitle({ children }: { children: ReactNode }) {
  const isMobile = useIsMobile();
  if (!isMobile) return null;
  return <div className="mobile-title">{children}</div>;
}
