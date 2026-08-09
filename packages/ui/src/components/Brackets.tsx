/** Neon corner brackets overlaid on the active nav item and reply cards. */
export function Brackets({ card = false }: { card?: boolean }) {
  const corner = card ? "absolute h-[11px] w-[11px]" : "absolute h-[7px] w-[7px]";
  return (
    <span
      className={`pointer-events-none absolute ${card ? "-inset-px" : "-inset-[3px]"}`}
      aria-hidden
    >
      <i
        className={`${corner} border-t-[1.5px] border-l-[1.5px] border-t-neon border-l-neon ${
          card ? "-top-px -left-px rounded-tl-[3px]" : "top-0 left-0"
        }`}
      />
      <i
        className={`${corner} border-t-[1.5px] border-r-[1.5px] border-t-neon border-r-neon ${
          card ? "-top-px -right-px rounded-tr-[3px]" : "top-0 right-0"
        }`}
      />
      <i
        className={`${corner} border-b-[1.5px] border-l-[1.5px] border-b-neon border-l-neon ${
          card ? "-bottom-px -left-px rounded-bl-[3px]" : "bottom-0 left-0"
        }`}
      />
      <i
        className={`${corner} border-r-[1.5px] border-b-[1.5px] border-r-neon border-b-neon ${
          card ? "-right-px -bottom-px rounded-br-[3px]" : "right-0 bottom-0"
        }`}
      />
    </span>
  );
}
