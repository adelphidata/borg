/** Neon corner brackets overlaid on the active nav item and reply cards. */
export function Brackets({ card = false }: { card?: boolean }) {
  return (
    <span className={card ? "brackets card-brackets" : "brackets"} aria-hidden>
      <i className="tl" />
      <i className="tr" />
      <i className="bl" />
      <i className="br" />
    </span>
  );
}
