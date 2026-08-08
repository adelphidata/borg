import { useNavigate } from "react-router-dom";
import { ChevronIcon } from "./icons";

/** "‹ Connections"-style return link at the top of a detail page. */
export function BackLink({ to, label }: { to: string; label: string }) {
  const navigate = useNavigate();
  return (
    <div
      className="flex cursor-pointer items-center gap-[7px] text-[10px] leading-none font-medium tracking-[0.16em] uppercase text-ink3 hover:text-ink"
      onClick={() => navigate(to)}
    >
      <ChevronIcon size={12} style={{ transform: "rotate(180deg)" }} />
      <span>{label}</span>
    </div>
  );
}
