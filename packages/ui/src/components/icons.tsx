import type { SVGProps } from "react";
import {
  Archive,
  ArrowUp,
  CalendarClock,
  Check,
  ChevronDown,
  ChevronRight,
  Copy,
  GitBranch,
  ListTree,
  MessageCircle,
  Mic,
  Moon,
  MoreHorizontal,
  Paperclip,
  Pencil,
  Plug,
  Plus,
  Search,
  Settings,
  ShieldCheck,
  Shuffle,
  Square,
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

/** Borg reticle logo. `full` adds the cross ticks used in the app header. */
export function LogoIcon({ size, full = false, ...props }: IconProps & { full?: boolean }) {
  const s = size ?? 20;
  return (
    <svg width={s} height={s} viewBox="0 0 24 24" fill="none" {...props}>
      <circle cx="12" cy="12" r="10.2" stroke={NEON} strokeWidth={full ? 1.3 : 1.6} />
      <circle cx="12" cy="12" r="3.4" fill={NEON} />
      {full && (
        <path d="M12 0v4.2M12 19.8V24M0 12h4.2M19.8 12H24" stroke={NEON} strokeWidth={1.3} />
      )}
    </svg>
  );
}

export const ChatIcon = lucide(MessageCircle, 16);
export const PlugIcon = lucide(Plug, 16);
export const ShieldIcon = lucide(ShieldCheck, 16);
export const GearIcon = lucide(Settings, 16);
export const ChevronIcon = lucide(ChevronRight, 13, 2);
export const ChevronDownIcon = lucide(ChevronDown, 13, 2);
export const CopyIcon = lucide(Copy, 14);
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
export const ScheduleIcon = lucide(CalendarClock, 16);
export const UsageIcon = lucide(Wallet, 16);
export const AgentsIcon = lucide(ListTree, 16);
export const SunIcon = lucide(Sun, 15);
export const MoonIcon = lucide(Moon, 15);
export const ShuffleIcon = lucide(Shuffle, 15);
export const SpeakerIcon = lucide(Volume2, 15);
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
