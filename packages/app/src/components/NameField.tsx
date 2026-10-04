import { ShuffleIcon, isSubmitEnter } from "@borg/ui";

/** The reroll control beside every name field. */
export function ShuffleButton({ onClick, disabled }: { onClick: () => void; disabled?: boolean }) {
  return (
    <button
      type="button"
      aria-label="Pick another name"
      title="Pick another name"
      disabled={disabled}
      onClick={onClick}
      className="flex-none cursor-pointer rounded-[7px] border-0 bg-transparent p-1.5 text-ink3 hover:bg-surface3 hover:text-ink disabled:cursor-default disabled:opacity-50"
    >
      <ShuffleIcon size={15} />
    </button>
  );
}

/**
 * A name that reads as part of the sentence around it: the input takes the
 * surrounding font and grows with its text, with a dashed underline as the
 * only sign it can be typed over.
 */
export function InlineNameField({
  value,
  onChange,
  onShuffle,
  onSubmit,
}: {
  value: string;
  onChange: (value: string) => void;
  onShuffle: () => void;
  onSubmit?: () => void;
}) {
  return (
    <span className="inline-flex max-w-full items-center gap-1 align-baseline">
      {/* A hidden twin of the text sizes the input: one grid cell, both in it. */}
      <span className="inline-grid min-w-0">
        <span aria-hidden className="invisible col-start-1 row-start-1 whitespace-pre">
          {value || "Name"}
        </span>
        <input
          aria-label="Agent name"
          value={value}
          placeholder="Name"
          spellCheck={false}
          maxLength={40}
          // An input's default 20-character width would win the grid cell;
          // size 1 hands the width to the hidden twin.
          size={1}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => onSubmit && isSubmitEnter(e) && onSubmit()}
          className="col-start-1 row-start-1 w-full min-w-0 border-0 border-b border-dashed border-line bg-transparent p-0 text-inherit outline-none select-text placeholder:text-ink3 focus:border-neon"
        />
      </span>
      <ShuffleButton onClick={onShuffle} />
    </span>
  );
}
