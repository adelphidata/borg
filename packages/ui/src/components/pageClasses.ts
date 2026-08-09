/** Class strings shared across pages (the Tailwind successors to .section-label & co). */

export const SECTION_LABEL =
  "text-[9.5px] leading-none font-medium tracking-[0.16em] uppercase text-ink3 mb-3.5";

/** Narrow column used by the connection + setting detail pages. */
export const DETAIL_COL = "flex max-w-[620px] flex-col gap-7";

/** Scrollable page body. Mobile padding is uniform; desktop varies slightly per page. */
export const scrollCol = (isMobile: boolean, desktopPadding: string) =>
  `flex-1 overflow-y-auto ${isMobile ? "px-[22px] py-4" : desktopPadding}`;
