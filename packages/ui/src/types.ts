import type { ComponentType } from "react";
import type { IconProps } from "./components/icons";

/**
 * One navigable destination. Apps own the route list; `@borg/ui` only renders it.
 * Sidebar, mobile TabBar, and the router all read from the same array — add a
 * route in the app and it appears in every surface.
 */
export interface AppRoute {
  id: string;
  path: string;
  /** Sidebar label */
  label: string;
  /** Bottom tab-bar label (mobile) */
  tabLabel: string;
  icon: ComponentType<IconProps>;
  page: ComponentType;
  /** Sidebar section: primary sits above the divider, secondary below */
  section: "primary" | "secondary";
  /** Red attention dot next to the nav label */
  hasAlert?: boolean;
  /**
   * Where this route is reachable from. A route the sidebar already covers
   * another way — the conversation list, which the sidebar shows inline — is
   * still a real page, and on mobile it is the only way in.
   */
  nav?: { sidebar?: boolean; tabBar?: boolean };
}

/** Copy for an `AlertBanner`. Apps supply it; the banner only renders it. */
export interface BannerAlert {
  /** Headline for the inline (horizontal) layout. */
  title: string;
  /** Headline for the stacked (narrow-column) layout. */
  stackedTitle: string;
  sub: string;
  /** Call-to-action button label. */
  action: string;
}

/** One recent conversation in the sidebar. Apps supply the list and handle opens. */
export interface RecentItem {
  id: string;
  title: string;
}

/** Signed-in identity shown in the sidebar footer. */
export interface NavUser {
  name: string;
  initials: string;
}
