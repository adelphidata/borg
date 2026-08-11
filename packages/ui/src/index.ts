/**
 * `@borg/ui` — the Borg design system.
 *
 * Presentational only: every component takes its data via props, so this package
 * has no dependency on the daemon, the route table, or any app state. Both
 * `apps/desktop` and `apps/web` render the same components from here.
 */

export type { AppRoute, BannerAlert, NavUser, RecentItem } from "./types";

export { AlertBanner } from "./components/AlertBanner";
export { BackLink } from "./components/BackLink";
export { Badge } from "./components/Badge";
export { Brackets } from "./components/Brackets";
export { CodeBlock } from "./components/CodeBlock";
export { Markdown } from "./components/Markdown";
export { MobileTitle } from "./components/MobileTitle";
export { PageChrome } from "./components/PageChrome";
export { Pill } from "./components/Pill";
export { StatusDot, type DotTone } from "./components/StatusDot";
export { Meter } from "./components/Meter";
export { Menu, clampToViewport, type MenuAction, type MenuProps } from "./components/Menu";
export {
  CompletionList,
  rowDomId,
  type CompletionRow,
  type CompletionListProps,
} from "./components/CompletionList";
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
  SECTION_LABEL_TEXT,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
} from "./components/pageClasses";

export { Sidebar } from "./nav/Sidebar";
export type { SidebarProps } from "./nav/Sidebar";
export { TabBar } from "./nav/TabBar";

export {
  THEME_PREFERENCES,
  ThemeProvider,
  resolveTheme,
  readPreference,
  useTheme,
} from "./lib/theme";
export type { Theme, ThemePreference } from "./lib/theme";
export { useIsMobile } from "./lib/useIsMobile";
export { STORAGE_KEYS } from "./lib/storageKeys";
