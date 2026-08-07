import type { SVGProps } from "react";

export type IconProps = SVGProps<SVGSVGElement> & { size?: number };

const NEON = "var(--neon)";

function base(size: number | undefined, fallback: number, props: IconProps) {
  // `size` is not a DOM attribute — strip it so a stray pass-through can't reach <svg>.
  const { size: _size, ...rest } = props;
  return {
    width: size ?? fallback,
    height: size ?? fallback,
    viewBox: "0 0 24 24",
    fill: "none",
    ...rest,
  };
}

const stroked = (size: number | undefined, fallback: number, props: IconProps) => ({
  ...base(size, fallback, props),
  stroke: "currentColor",
  strokeWidth: 1.6,
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
});

/** Borg reticle logo. `full` adds the cross ticks used in the app header. */
export function LogoIcon({ size, full = false, ...props }: IconProps & { full?: boolean }) {
  return (
    <svg {...base(size, 20, props)}>
      <circle cx="12" cy="12" r="10.2" stroke={NEON} strokeWidth={full ? 1.3 : 1.6} />
      <circle cx="12" cy="12" r="3.4" fill={NEON} />
      {full && (
        <path d="M12 0v4.2M12 19.8V24M0 12h4.2M19.8 12H24" stroke={NEON} strokeWidth={1.3} />
      )}
    </svg>
  );
}

export function ChatIcon({ size, ...props }: IconProps) {
  return (
    <svg {...stroked(size, 16, props)}>
      <path d="M21 11.5a8.4 8.4 0 0 1-9 8.4 9 9 0 0 1-3.9-.9L3 21l1.9-4.6A8.4 8.4 0 0 1 12 3.1a8.4 8.4 0 0 1 9 8.4Z" />
    </svg>
  );
}

export function PlugIcon({ size, ...props }: IconProps) {
  return (
    <svg {...stroked(size, 16, props)}>
      <path d="M9 3v6M15 3v6M6 9h12v4a6 6 0 0 1-6 6 6 6 0 0 1-6-6V9ZM12 19v3" />
    </svg>
  );
}

export function ShieldIcon({ size, ...props }: IconProps) {
  return (
    <svg {...stroked(size, 16, props)}>
      <path d="M12 21s7-4 7-9.5V6l-7-3-7 3v5.5C5 17 12 21 12 21Z" />
      <path d="m9.2 11.8 2 2 3.6-3.8" />
    </svg>
  );
}

export function GearIcon({ size, ...props }: IconProps) {
  return (
    <svg {...stroked(size, 16, props)}>
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-2.9 1.2v.2a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-3-1.2l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1A1.7 1.7 0 0 0 3 15a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.2-3l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 3-1.2V4a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 3 1.2l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0 1.2 3H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z" />
    </svg>
  );
}

export function SendIcon({ size, ...props }: IconProps) {
  return (
    // Fixed dark stroke: this icon sits on the neon send button, so currentColor won't contrast.
    <svg {...stroked(size, 14, props)} stroke="#0C100D" strokeWidth={2.3}>
      <path d="M12 19V5M5 12l7-7 7 7" />
    </svg>
  );
}

export function CheckIcon({ size, ...props }: IconProps) {
  return (
    <svg {...stroked(size, 22, props)} stroke={NEON} strokeWidth={2.2}>
      <path d="M4 12.5 9.5 18 20 6.5" />
    </svg>
  );
}

export function ChevronIcon({ size, ...props }: IconProps) {
  return (
    <svg {...stroked(size, 13, props)} strokeWidth={2}>
      <path d="m9 6 6 6-6 6" />
    </svg>
  );
}
