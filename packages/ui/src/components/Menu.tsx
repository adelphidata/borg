import { Fragment, useEffect, useRef, type ReactNode } from "react";

/**
 * The row action menu.
 *
 * Controlled rather than self-opening: Escape belongs to the app's stacked
 * keybind registry, which lives above this package, so the caller owns `open`
 * and this owns only what a menu looks like and where it sits.
 */

export interface MenuAction {
  id: string;
  label: string;
  icon?: ReactNode;
  onSelect: () => void;
  /** Draws a divider above this item — for an action that undoes something. */
  dividerBefore?: boolean;
}

export interface MenuProps {
  open: boolean;
  onClose: () => void;
  actions: MenuAction[];
  /** Names the menu for a screen reader, e.g. "Conversation actions". */
  label: string;
  /**
   * Where a right-click opened it, in viewport coordinates. Omit and the menu
   * hangs under its trigger instead, which needs a positioned ancestor.
   */
  at?: { x: number; y: number };
}

const WIDTH_PX = 150;
const ITEM_HEIGHT_PX = 28;
const EDGE_GAP_PX = 8;

const ITEM =
  "flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent px-3 py-[7px] text-left text-[12.5px] leading-none text-ink2 hover:bg-surface3 hover:text-ink";

/**
 * Keeps the menu on screen when it opens near an edge. Height is estimated
 * from the item count rather than measured — measuring would need a layout
 * pass the menu has not had yet, and being a few pixels off is invisible next
 * to opening half outside the window.
 */
export function clampToViewport(
  at: { x: number; y: number },
  itemCount: number,
  viewport: { width: number; height: number },
): { left: number; top: number } {
  const height = itemCount * ITEM_HEIGHT_PX + EDGE_GAP_PX * 2;
  return {
    left: Math.max(EDGE_GAP_PX, Math.min(at.x, viewport.width - WIDTH_PX - EDGE_GAP_PX)),
    top: Math.max(EDGE_GAP_PX, Math.min(at.y, viewport.height - height - EDGE_GAP_PX)),
  };
}

export function Menu({ open, onClose, actions, label, at }: MenuProps) {
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) onClose();
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open, onClose]);

  if (!open) return null;

  const position = at
    ? clampToViewport(at, actions.length, {
        width: window.innerWidth,
        height: window.innerHeight,
      })
    : undefined;

  return (
    <div
      ref={root}
      role="menu"
      aria-label={label}
      // The row underneath opens the conversation; the menu must not.
      onClick={(e) => e.stopPropagation()}
      style={position ? { left: position.left, top: position.top } : undefined}
      className={`z-30 w-[150px] overflow-hidden rounded-[9px] border border-line bg-surface2 py-1 shadow-(--frameShadow) ${
        position ? "fixed" : "absolute top-7 right-0"
      }`}
    >
      {actions.map((action) => (
        // A fragment rather than a wrapper element: a `menu` has to own its
        // `menuitem`s directly, and an intervening div costs a screen reader
        // the item count and the arrow-key walk through the list.
        <Fragment key={action.id}>
          {action.dividerBefore && <div role="separator" className="my-1 h-px bg-line2" />}
          <button
            type="button"
            role="menuitem"
            className={ITEM}
            onClick={() => {
              onClose();
              action.onSelect();
            }}
          >
            {action.icon ?? <span className="w-[13px]" />} {action.label}
          </button>
        </Fragment>
      ))}
    </div>
  );
}
