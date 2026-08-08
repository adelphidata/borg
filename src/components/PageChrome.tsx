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
    <main className="flex min-w-0 flex-1 flex-col bg-surface">
      {!isMobile && (
        <header className="flex h-14 flex-none items-center gap-3 border-b border-line2 px-[30px]">
          <span className="text-[14px] leading-none font-medium tracking-[-0.01em] text-ink">
            {title}
          </span>
          <span className="flex-1" />
          {meta && <span className="text-[11.5px] leading-none text-ink3">{meta}</span>}
        </header>
      )}
      {isMobile && mobileBrandHeader && (
        <header className="flex flex-none items-center gap-2.5 border-b border-line2 px-[22px] pt-[calc(10px+env(safe-area-inset-top))] pb-3.5">
          <LogoIcon size={17} />
          <span className="text-[12.5px] leading-none font-semibold tracking-[0.2em] text-ink">
            BORG
          </span>
          <span style={{ flex: 1 }} />
          <span className="h-1.5 w-1.5 animate-bpulse rounded-full bg-neon shadow-(--glow)" />
        </header>
      )}
      {children}
    </main>
  );
}
