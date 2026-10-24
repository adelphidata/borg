import { useState } from "react";
import {
  ArchiveIcon,
  Menu,
  MoreIcon,
  PencilIcon,
  PinIcon,
  UnpinIcon,
  type MenuAction,
} from "@borg/ui";
import { useKeybind } from "../../lib/keybinds";

export interface SessionRowMenuProps {
  onRename: () => void;
  onArchive: () => void;
  /** Brings an archived conversation back. Offered in place of Archive. */
  onRestore?: () => void;
  onExport: (format: string) => void;
  /** Whether this row is already archived. */
  archived?: boolean;
  /** Whether this row is pinned to the top of the lists. */
  pinned?: boolean;
  /** Pins or unpins the row; without it the menu offers neither. */
  onPin?: (pinned: boolean) => void;
}

/** Builds the row's actions once, so the kebab and right-click can never differ. */
export function sessionRowActions({
  onRename,
  onArchive,
  onRestore,
  onExport,
  archived = false,
  pinned = false,
  onPin,
}: SessionRowMenuProps): MenuAction[] {
  return [
    { id: "rename", label: "Rename", icon: <PencilIcon size={13} />, onSelect: onRename },
    // An archived row is out of both lists, so there is nothing to pin it to.
    ...(onPin && !archived
      ? [
          {
            id: "pin",
            label: pinned ? "Unpin" : "Pin",
            icon: pinned ? <UnpinIcon size={13} /> : <PinIcon size={13} />,
            onSelect: () => onPin(!pinned),
          },
        ]
      : []),
    { id: "export", label: "Export as Markdown", onSelect: () => onExport("md") },
    // The same slot either way: a row never grows an action, it changes the one
    // it has, so the menu is the same shape wherever the conversation sits.
    archived
      ? {
          id: "restore",
          label: "Restore",
          icon: <ArchiveIcon size={13} />,
          onSelect: onRestore ?? (() => {}),
          dividerBefore: true,
        }
      : {
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
        // The menu's outside-click listener sits on `document` and the trigger
        // is outside the menu, so without this a second press would close on
        // pointerdown and the click that follows would immediately reopen it —
        // the kebab could never shut its own menu. Stopping here keeps the
        // toggle on `onClick`, which keyboard activation also fires.
        onPointerDown={(e) => e.stopPropagation()}
        onClick={(e) => {
          // The row underneath opens the conversation; the menu must not.
          e.stopPropagation();
          setOpen((v) => !v);
        }}
        className="flex h-6 w-6 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover-reveal hover:text-ink"
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
