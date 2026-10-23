import type { SVGProps } from "react";
import {
  Archive,
  ArrowUp,
  CalendarClock,
  Check,
  ChevronDown,
  ChevronRight,
  CircleArrowUp,
  Copy,
  GitBranch,
  Info,
  ListTree,
  MessageCircle,
  Mic,
  Moon,
  MoreHorizontal,
  Paperclip,
  Pencil,
  Pin,
  PinOff,
  Plug,
  Plus,
  Search,
  Server,
  Settings,
  ShieldCheck,
  Shuffle,
  Sparkles,
  Square,
  SquarePen,
  Sun,
  Trash2,
  Volume2,
  Wallet,
  X,
  type LucideIcon,
} from "lucide-react";

export type IconProps = SVGProps<SVGSVGElement> & { size?: number };

const NEON = "var(--neon)";

/** Default stroke weight for factory-built icons, matching the app's 1.6px line style. */
const STROKE = 1.6;

function lucide(Icon: LucideIcon, fallback: number, strokeWidth = STROKE) {
  function WrappedIcon({ size, ...props }: IconProps) {
    return <Icon size={size ?? fallback} strokeWidth={strokeWidth} {...props} />;
  }
  WrappedIcon.displayName = Icon.displayName ?? "Icon";
  return WrappedIcon;
}

// A URL rather than an import, so no package needs an image module type: each
// app's Vite build emits the file and rewrites the path.
const HEAD_SRC = new URL("../assets/borg-head.webp", import.meta.url).href;
/** The cut-out is 384×264. */
const HEAD_ASPECT = 264 / 384;

/**
 * Borg's face, the brand mark. Decorative: wherever it shows, the name next to
 * it or the greeting under it already says who this is.
 */
export function BorgHead({ width, className }: { width: number; className?: string }) {
  return (
    <img
      src={HEAD_SRC}
      alt=""
      width={width}
      height={Math.round(width * HEAD_ASPECT)}
      draggable={false}
      className={className}
    />
  );
}

export const ChatIcon = lucide(MessageCircle, 16);
export const PlugIcon = lucide(Plug, 16);
export const ShieldIcon = lucide(ShieldCheck, 16);
export const GearIcon = lucide(Settings, 16);
export const ChevronIcon = lucide(ChevronRight, 13, 2);
export const ChevronDownIcon = lucide(ChevronDown, 13, 2);
export const CopyIcon = lucide(Copy, 14);
export const UpdateIcon = lucide(CircleArrowUp, 16);
export const SearchIcon = lucide(Search, 15);
export const PlusIcon = lucide(Plus, 15);
export const CloseIcon = lucide(X, 15);
export const AttachIcon = lucide(Paperclip, 15);
export const MicIcon = lucide(Mic, 15);
export const MoreIcon = lucide(MoreHorizontal, 15);
export const TrashIcon = lucide(Trash2, 14);
export const PencilIcon = lucide(Pencil, 14);
export const BranchIcon = lucide(GitBranch, 14);
export const ArchiveIcon = lucide(Archive, 14);
export const PinIcon = lucide(Pin, 14);
export const UnpinIcon = lucide(PinOff, 14);
export const NewChatIcon = lucide(SquarePen, 14);
export const ScheduleIcon = lucide(CalendarClock, 16);
export const UsageIcon = lucide(Wallet, 16);
export const AgentsIcon = lucide(ListTree, 16);
export const SunIcon = lucide(Sun, 15);
export const MoonIcon = lucide(Moon, 15);
export const ShuffleIcon = lucide(Shuffle, 15);
export const ServerIcon = lucide(Server, 16);
export const SparklesIcon = lucide(Sparkles, 16);
export const SpeakerIcon = lucide(Volume2, 15);
export const InfoIcon = lucide(Info, 13);
export const StopIcon = lucide(Square, 13, 2);

export function SendIcon({ size, ...props }: IconProps) {
  return (
    // Fixed dark stroke: this icon sits on the neon send button, so currentColor won't contrast.
    <ArrowUp size={size ?? 14} {...props} strokeWidth={2.3} stroke="#0C100D" />
  );
}

export function CheckIcon({ size, ...props }: IconProps) {
  return <Check size={size ?? 22} {...props} strokeWidth={2.2} stroke={NEON} />;
}
