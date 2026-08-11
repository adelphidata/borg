import { useState } from "react";
import { ArchiveIcon, Menu, MoreIcon, PencilIcon, type MenuAction } from "@borg/ui";
import { useKeybind } from "../../lib/keybinds";

export interface SessionRowMenuProps {
  onRename: () => void;
  onArchive: () => void;
  onExport: (format: string) => void;
}

/** Builds the row's actions once, so the kebab and right-click can never differ. */
export function sessionRowActions({
  onRename,
  onArchive,
  onExport,
}: SessionRowMenuProps): MenuAction[] {
  return [
    { id: "rename", label: "Rename", icon: <PencilIcon size={13} />, onSelect: onRename },
    { id: "export", label: "Export…", onSelect: () => onExport("md") },
    {
      id: "archive",
      label: "Archive",
      icon: <ArchiveIcon size={13} />,
      onSelect: onArchive,
      dividerBefore: true,
    },
  ];
}

/**
 * Escape closes an open menu, through the registry rather than a raw listener:
 * the registry is stacked, so the menu takes Escape and hands it back on close.
 * A bare listener would close the menu *and* let the composer's cancel through,
 * killing a running turn with the same keypress.
 */
export function useMenuEscape(open: boolean, close: () => void) {
  useKeybind("cancel-turn", close, { enabled: open, inInputs: true });
}

/** The per-row overflow menu. Closes on outside click and on Escape. */
export function SessionRowMenu(props: SessionRowMenuProps) {
  const [open, setOpen] = useState(false);
  useMenuEscape(open, () => setOpen(false));

  return (
    <div className="relative flex-none">
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
      <Menu
        open={open}
        onClose={() => setOpen(false)}
        label="Conversation actions"
        actions={sessionRowActions(props)}
      />
    </div>
  );
}
