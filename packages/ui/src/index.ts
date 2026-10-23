/**
 * `@borg/ui` — the Borg design system.
 *
 * Presentational only: every component takes its data via props, so this package
 * has no dependency on the daemon, the route table, or any app state. Both
 * `apps/desktop` and `apps/web` render the same components from here.
 */

export type { AppRoute, BannerAlert, NavUser, RecentItem, SidebarUpdate } from "./types";

export { isImeKey, isSubmitEnter, type ComposableKeyEvent } from "./lib/imeKey";
export { AlertBanner } from "./components/AlertBanner";
export { BackLink } from "./components/BackLink";
export { Badge } from "./components/Badge";
export { Brackets } from "./components/Brackets";
export { CodeBlock } from "./components/CodeBlock";
export { FindBar, type FindBarProps } from "./components/FindBar";
export { Markdown } from "./components/Markdown";
export { MobileTitle } from "./components/MobileTitle";
export { PageChrome } from "./components/PageChrome";
export { Pill } from "./components/Pill";
export { StatusDot, type DotTone } from "./components/StatusDot";
export { Toast, type ToastProps } from "./components/Toast";
export { Truncated } from "./components/Truncated";
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
  BorgHead,
  BranchIcon,
  ChatIcon,
  CheckIcon,
  ChevronDownIcon,
  ChevronIcon,
  CloseIcon,
  CopyIcon,
  GearIcon,
  InfoIcon,
  MicIcon,
  MoonIcon,
  MoreIcon,
  NewChatIcon,
  PencilIcon,
  PinIcon,
  PlugIcon,
  PlusIcon,
  ScheduleIcon,
  SearchIcon,
  SendIcon,
  ServerIcon,
  ShieldIcon,
  ShuffleIcon,
  SparklesIcon,
  SpeakerIcon,
  StopIcon,
  SunIcon,
  TrashIcon,
  UnpinIcon,
  UsageIcon,
} from "./components/icons";
export type { IconProps } from "./components/icons";
export {
  CONVERSATION_COL,
  DETAIL_COL,
  SECTION_LABEL,
  SECTION_LABEL_TEXT,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  TEXT_FIELD,
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
export { usePauseAnimationsWhenHidden } from "./lib/pauseWhenHidden";
export { STORAGE_KEYS } from "./lib/storageKeys";
