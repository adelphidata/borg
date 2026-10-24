import { useState, type ReactNode } from "react";
import { isSubmitEnter } from "../lib/imeKey";
import { useIsMobile } from "../lib/useIsMobile";
import { BorgHead } from "./icons";

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
   * Makes the title editable: a double-click (or Enter on it) opens a box in
   * its place, Enter saves and Escape puts the old name back. Resolves to
   * false when the save did not take, so the box stays open with the text.
   */
  onRename?: (title: string) => Promise<boolean> | boolean | void;
  /** Accessible name of the rename box. Defaults to "Conversation name". */
  renameLabel?: string;
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
/** The header title as a thing you can double-click to rename. */
function RenamableTitle({
  title,
  onRename,
  label,
}: {
  title: string;
  onRename: NonNullable<PageChromeProps["onRename"]>;
  label: string;
}) {
  const [draft, setDraft] = useState<string>();
  const [saving, setSaving] = useState(false);
  const editing = draft !== undefined;
  const commit = () => {
    if (draft === undefined || saving) return;
    const next = draft.trim();
    if (next === "" || next === title) {
      setDraft(undefined);
      return;
    }
    setSaving(true);
    void Promise.resolve(onRename(next))
      .then((ok) => {
        if (ok !== false) setDraft(undefined);
      })
      .finally(() => setSaving(false));
  };
  if (editing) {
    return (
      <input
        autoFocus
        aria-label={label}
        className="m-0 min-w-0 max-w-[420px] flex-1 rounded-[7px] border border-focus bg-surface2 px-2 py-1 text-[14px] leading-none font-medium tracking-[-0.01em] text-ink outline-none"
        value={draft}
        disabled={saving}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            setDraft(undefined);
          } else if (isSubmitEnter(e)) {
            e.preventDefault();
            commit();
          }
        }}
      />
    );
  }
  return (
    <h1
      // Focusable so the keyboard can reach the rename; Enter does what a
      // double-click does.
      tabIndex={0}
      title="Double-click to rename"
      className="m-0 cursor-text rounded-[7px] px-1 py-1 text-[14px] leading-none font-medium tracking-[-0.01em] text-ink hover:bg-surface2 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus"
      onDoubleClick={() => setDraft(title)}
      onKeyDown={(e) => {
        if (e.key === "Enter") setDraft(title);
      }}
    >
      {title}
    </h1>
  );
}

export function PageChrome({
  title,
  meta,
  mobileBrandHeader = false,
  headerActions,
  onRename,
  renameLabel = "Conversation name",
  aside,
  children,
}: PageChromeProps) {
  const isMobile = useIsMobile();

  const page = (
    <main className="flex min-w-0 flex-1 flex-col bg-surface">
      {!isMobile && (
        <header className="flex h-14 flex-none items-center gap-3 border-b border-line2 px-[30px]">
          {onRename ? (
            <RenamableTitle title={title} onRename={onRename} label={renameLabel} />
          ) : (
            <h1 className="m-0 text-[14px] leading-none font-medium tracking-[-0.01em] text-ink">
              {title}
            </h1>
          )}
          <span className="flex-1" />
          {meta && <span className="text-[11.5px] leading-none text-ink3">{meta}</span>}
          {headerActions}
        </header>
      )}
      {isMobile && mobileBrandHeader && (
        <header className="flex flex-none items-center gap-2.5 border-b border-line2 px-[22px] pt-[calc(10px+env(safe-area-inset-top))] pb-3.5">
          <BorgHead width={24} />
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
