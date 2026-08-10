/** Class strings shared across pages. */

/**
 * The uppercase micro-label, typography only.
 *
 * Kept separate from the spacing because appending `mb-0` to a string that
 * already carries `mb-3.5` does not reliably win — Tailwind resolves two
 * utilities of equal specificity by stylesheet order, not by their order in
 * the class attribute. A label that needs different spacing composes this with
 * its own margin instead.
 */
export const SECTION_LABEL_TEXT =
  "text-[9.5px] leading-none font-medium tracking-[0.16em] uppercase text-ink3";

/** The common case: a section label with the standard gap under it. */
export const SECTION_LABEL = `${SECTION_LABEL_TEXT} mb-3.5`;

/** Narrow column used by the connection + setting detail pages. */
export const DETAIL_COL = "flex max-w-[620px] flex-col gap-7";

/** Both settings pages share edge-to-edge 38px desktop padding. */
export const SETTINGS_SCROLL_DESKTOP_PADDING = "p-[38px]";

/** Scrollable page body. Mobile padding is uniform; desktop varies slightly per page. */
export const scrollCol = (isMobile: boolean, desktopPadding: string) =>
  `flex-1 overflow-y-auto ${isMobile ? "px-[22px] py-4" : desktopPadding}`;
