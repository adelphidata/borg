import { useEffect, useRef, useState } from "react";
import { ArchiveIcon, MoreIcon, PencilIcon } from "@borg/ui";

export interface SessionRowMenuProps {
  onRename: () => void;
  onArchive: () => void;
  onExport: (format: string) => void;
}

const ITEM =
  "flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent px-3 py-[7px] text-left text-[12.5px] leading-none text-ink2 hover:bg-surface3 hover:text-ink";

/** The per-row overflow menu. Closes on outside click and on Escape. */
export function SessionRowMenu({ onRename, onArchive, onExport }: SessionRowMenuProps) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  const choose = (action: () => void) => () => {
    setOpen(false);
    action();
  };

  return (
    <div ref={root} className="relative flex-none">
      <button
        type="button"
        aria-label="Conversation actions"
        aria-expanded={open}
        onClick={(e) => {
          // The row underneath opens the conversation; the menu must not.
          e.stopPropagation();
          setOpen((v) => !v);
        }}
        className="flex h-6 w-6 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 hover:text-ink"
      >
        <MoreIcon size={14} />
      </button>
      {open && (
        <div
          onClick={(e) => e.stopPropagation()}
          className="absolute top-7 right-0 z-20 w-[150px] overflow-hidden rounded-[9px] border border-line bg-surface2 py-1 shadow-(--frameShadow)"
        >
          <button type="button" className={ITEM} onClick={choose(onRename)}>
            <PencilIcon size={13} /> Rename
          </button>
          <button type="button" className={ITEM} onClick={choose(() => onExport("md"))}>
            <span className="w-[13px]" /> Export…
          </button>
          <div className="my-1 h-px bg-line2" />
          <button type="button" className={ITEM} onClick={choose(onArchive)}>
            <ArchiveIcon size={13} /> Archive
          </button>
        </div>
      )}
    </div>
  );
}
