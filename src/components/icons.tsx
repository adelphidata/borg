import type { SVGProps } from "react";
import {
  ArrowUp,
  Check,
  ChevronRight,
  MessageCircle,
  Plug,
  Settings,
  ShieldCheck,
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

export function SendIcon({ size, ...props }: IconProps) {
  return (
    // Fixed dark stroke: this icon sits on the neon send button, so currentColor won't contrast.
    <ArrowUp size={size ?? 14} {...props} strokeWidth={2.3} stroke="#0C100D" />
  );
}

export function CheckIcon({ size, ...props }: IconProps) {
  return <Check size={size ?? 22} {...props} strokeWidth={2.2} stroke={NEON} />;
}
