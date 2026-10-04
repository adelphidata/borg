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
  /** Controls in the desktop header, right of the metadata. */
  headerActions?: ReactNode;
  /**
   * A companion panel alongside the page. Desktop only — at phone width there
   * is no room for two columns, and the caller should not have to know that.
   */
  aside?: ReactNode;
  children: ReactNode;
}

/**
 * Shared page frame: 56px header with title + meta on desktop; on mobile either
 * the brand header (conversation) or nothing — pages render their own heading
 * via <MobileTitle>.
 */
export function PageChrome({
  title,
  meta,
  mobileBrandHeader = false,
  headerActions,
  aside,
  children,
}: PageChromeProps) {
  const isMobile = useIsMobile();

  const page = (
    <main className="flex min-w-0 flex-1 flex-col bg-surface">
      {!isMobile && (
        <header className="flex h-14 flex-none items-center gap-3 border-b border-line2 px-[30px]">
          <h1 className="m-0 text-[14px] leading-none font-medium tracking-[-0.01em] text-ink">
            {title}
          </h1>
          <span className="flex-1" />
          {meta && <span className="text-[11.5px] leading-none text-ink3">{meta}</span>}
          {headerActions}
        </header>
      )}
      {isMobile && mobileBrandHeader && (
        <header className="flex flex-none items-center gap-2.5 border-b border-line2 px-[22px] pt-[calc(10px+env(safe-area-inset-top))] pb-3.5">
          <LogoIcon size={17} />
          <span className="text-[12.5px] leading-none font-semibold tracking-[0.2em] text-ink">
            BORG
          </span>
          <span className="flex-1" />
          <span className="h-1.5 w-1.5 animate-bpulse rounded-full bg-neon shadow-(--glow)" />
        </header>
      )}
      {/* At phone width there is no title bar, so the page keeps its name as
          a heading for assistive tech only. A visible <MobileTitle> repeats
          it for the eye and is hidden from assistive tech. */}
      {isMobile && <h1 className="sr-only">{title}</h1>}
      {children}
    </main>
  );

  if (!aside || isMobile) return page;
  return (
    <div className="flex min-w-0 flex-1">
      {page}
      {aside}
    </div>
  );
}
