import { ChatIcon, GearIcon, PlugIcon, ScheduleIcon, ShieldIcon, UsageIcon } from "@borg/ui";
import type { AppRoute } from "@borg/ui";
import { ConversationPage } from "./features/conversation/ConversationPage";
import { ConnectionsPage } from "./features/connections/ConnectionsPage";
import { HealthPage } from "./features/health/HealthPage";
import { SchedulesPage } from "./features/schedules/SchedulesPage";
import { SessionsPage } from "./features/sessions/SessionsPage";
import { SettingsPage } from "./features/settings/SettingsPage";
import { UsagePage } from "./features/usage/UsagePage";

// `AppRoute` is defined in `@borg/ui` because Sidebar and TabBar render it.
// Re-exported so features can import the route type alongside the route list.
export type { AppRoute };

/**
 * Single source of truth for navigation. Sidebar, mobile tab bar, and the
 * router all render from this list — add a page here and it appears everywhere.
 */
export const routes: AppRoute[] = [
  {
    id: "conversation",
    path: "/",
    label: "Chats",
    tabLabel: "Chat",
    icon: ChatIcon,
    page: ConversationPage,
    section: "primary",
  },
  {
    id: "sessions",
    path: "/sessions",
    label: "Conversations",
    tabLabel: "Chats",
    icon: ChatIcon,
    page: SessionsPage,
    section: "primary",
    // The sidebar lists recents inline, so a nav row for the same thing would
    // be noise. On mobile there is no sidebar, which makes this tab the only
    // route to the conversation list.
    nav: { sidebar: false },
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
    id: "schedules",
    path: "/schedules",
    label: "Scheduled",
    tabLabel: "Tasks",
    icon: ScheduleIcon,
    page: SchedulesPage,
    section: "primary",
    // Five tabs is the ceiling on a phone. Both of these stay off the tab bar
    // and are reached from More, or from the palette on a keyboard.
    nav: { tabBar: false },
  },
  {
    id: "usage",
    path: "/usage",
    label: "Usage",
    tabLabel: "Usage",
    icon: UsageIcon,
    page: UsagePage,
    section: "secondary",
    nav: { tabBar: false },
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
