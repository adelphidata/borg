/**
 * `@borg/ui` — the Borg design system.
 *
 * Presentational only: every component takes its data via props, so this package
 * has no dependency on the daemon, the route table, or any app state. Both
 * `apps/desktop` and `apps/web` render the same components from here.
 */

export type { AppRoute, BannerAlert, NavUser } from "./types";

export { AlertBanner } from "./components/AlertBanner";
export { BackLink } from "./components/BackLink";
export { Badge } from "./components/Badge";
export { Brackets } from "./components/Brackets";
export { CodeBlock } from "./components/CodeBlock";
export { Markdown } from "./components/Markdown";
export { MobileTitle } from "./components/MobileTitle";
export { PageChrome } from "./components/PageChrome";
export { Pill } from "./components/Pill";
export { StatusDot } from "./components/StatusDot";
export {
  AgentsIcon,
  ArchiveIcon,
  AttachIcon,
  BranchIcon,
  ChatIcon,
  CheckIcon,
  ChevronDownIcon,
  ChevronIcon,
  CloseIcon,
  CopyIcon,
  GearIcon,
  ImageIcon,
  LogoIcon,
  MoreIcon,
  PencilIcon,
  PlugIcon,
  PlusIcon,
  ScheduleIcon,
  SearchIcon,
  SendIcon,
  ShieldIcon,
  TrashIcon,
  UsageIcon,
} from "./components/icons";
export type { IconProps } from "./components/icons";
export {
  DETAIL_COL,
  SECTION_LABEL,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
} from "./components/pageClasses";

export { Sidebar } from "./nav/Sidebar";
export type { SidebarProps } from "./nav/Sidebar";
export { TabBar } from "./nav/TabBar";

export { ThemeProvider, useTheme } from "./lib/theme";
export type { Theme } from "./lib/theme";
export { useIsMobile } from "./lib/useIsMobile";
export { STORAGE_KEYS } from "./lib/storageKeys";
