import type { CSSProperties, ReactNode } from "react";
import { useIsMobile } from "../lib/useIsMobile";

/** In-column page heading rendered only at the mobile breakpoint. */
export function MobileTitle({ children, style }: { children: ReactNode; style?: CSSProperties }) {
  const isMobile = useIsMobile();
  if (!isMobile) return null;
  return (
    <div className="mobile-title" style={style}>
      {children}
    </div>
  );
}
