import type { ReactNode } from "react";
import { useIsMobile } from "../lib/useIsMobile";
import { LogoIcon } from "./icons";

interface PageChromeProps {
  /** Desktop header title */
  title: string;
  /** Desktop header right-side metadata */
  meta?: string;
  /** Conversation uses the BORG brand header on mobile instead of a plain title. */
  mobileBrandHeader?: boolean;
  children: ReactNode;
}

/**
 * Shared page frame: 56px header with title + meta on desktop; on mobile either
 * the brand header (conversation) or nothing — pages render their own heading
 * via <MobileTitle>.
 */
export function PageChrome({ title, meta, mobileBrandHeader = false, children }: PageChromeProps) {
  const isMobile = useIsMobile();

  return (
    <main className="page">
      {!isMobile && (
        <header className="page-header">
          <span className="title">{title}</span>
          <span className="spacer" />
          {meta && <span className="meta">{meta}</span>}
        </header>
      )}
      {isMobile && mobileBrandHeader && (
        <header className="mobile-header">
          <LogoIcon size={17} />
          <span className="brand">BORG</span>
          <span style={{ flex: 1 }} />
          <span className="pulse" />
        </header>
      )}
      {children}
    </main>
  );
}
