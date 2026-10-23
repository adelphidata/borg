/**
 * An on/off control for a list row.
 *
 * A real `role="switch"` button, so a screen reader says "on" or "off" and a
 * keyboard reaches it; the track and knob are drawn with the theme tokens so
 * the on state is the same neon that marks a healthy row elsewhere.
 */
export function Switch({
  on,
  onChange,
  label,
  disabled = false,
}: {
  on: boolean;
  onChange: (next: boolean) => void;
  /** Accessible name — the row's own title, e.g. "Enable git". */
  label: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      disabled={disabled}
      onClick={(e) => {
        e.stopPropagation();
        if (!disabled) onChange(!on);
      }}
      className={`relative h-[20px] w-[34px] flex-none cursor-pointer rounded-full border transition-[background,border-color] duration-150 disabled:cursor-default disabled:opacity-50 ${
        on ? "border-neon bg-neon" : "border-field bg-surface3"
      }`}
    >
      <span
        className={`absolute top-[2px] h-[14px] w-[14px] rounded-full bg-surface shadow-sm transition-[left] duration-150 ${
          on ? "left-[16px]" : "left-[2px]"
        }`}
      />
    </button>
  );
}
