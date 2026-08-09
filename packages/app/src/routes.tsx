import type { ComponentType } from "react";
import { ChatIcon, GearIcon, PlugIcon, ShieldIcon } from "@borg/ui";
import type { IconProps } from "@borg/ui";
import { ConversationPage } from "./features/conversation/ConversationPage";
import { ConnectionsPage } from "./features/connections/ConnectionsPage";
import { HealthPage } from "./features/health/HealthPage";
import { SettingsPage } from "./features/settings/SettingsPage";

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
}

/**
 * Single source of truth for navigation. Sidebar, mobile tab bar, and the
 * router all render from this list — add a page here and it appears everywhere.
 */
export const routes: AppRoute[] = [
  {
    id: "conversation",
    path: "/",
    label: "Conversation",
    tabLabel: "Chat",
    icon: ChatIcon,
    page: ConversationPage,
    section: "primary",
  },
  {
    id: "connections",
    path: "/connections",
    label: "Connections",
    tabLabel: "Links",
    icon: PlugIcon,
    page: ConnectionsPage,
    section: "primary",
  },
  {
    id: "health",
    path: "/health",
    label: "Health",
    tabLabel: "Health",
    icon: ShieldIcon,
    page: HealthPage,
    section: "secondary",
  },
  {
    id: "settings",
    path: "/settings",
    label: "Settings",
    tabLabel: "More",
    icon: GearIcon,
    page: SettingsPage,
    section: "secondary",
  },
];
