import { Badge, StatusDot, type DotTone } from "@borg/ui";

/** One connection as a card (desktop) or a row (mobile). */
export interface Card {
  id: string;
  badge: string;
  name: string;
  sub: string;
  tone: DotTone;
}

/** Desktop: connections as a card grid. */
export function ConnCards({ cards, open }: { cards: Card[]; open: (id: string) => void }) {
  return (
    <section>
      <div className="grid grid-cols-4 gap-3">
        {cards.map((c) => (
          <button
            type="button"
            key={c.id}
            className="cursor-pointer rounded-[11px] border border-line bg-surface2 p-4 text-left"
            onClick={() => open(c.id)}
          >
            <div className="mb-[11px] flex items-center justify-between">
              <Badge>{c.badge}</Badge>
              <StatusDot tone={c.tone} />
            </div>
            <div className="text-[13.5px] leading-[1.2] font-medium text-ink">{c.name}</div>
            <div className="mt-[5px] text-[11.5px] leading-[1.4] text-ink3">{c.sub}</div>
          </button>
        ))}
      </div>
    </section>
  );
}

/** Mobile: connections as list rows. */
export function ConnList({ cards, open }: { cards: Card[]; open: (id: string) => void }) {
  return (
    <div className="flex flex-col">
      {cards.map((c) => (
        <button
          type="button"
          key={c.id}
          className="flex w-full cursor-pointer items-center gap-[13px] border-b border-line2 py-3.5 text-left"
          onClick={() => open(c.id)}
        >
          <Badge sizeClasses="h-[30px] w-[30px] rounded-[9px] text-[10.5px]">{c.badge}</Badge>
          <div className="min-w-0 flex-1">
            <div className="text-[13.5px] leading-[1.25] font-medium text-ink">{c.name}</div>
            <div className="mt-[3px] text-[11.5px] leading-[1.3] text-ink3">{c.sub}</div>
          </div>
          <StatusDot tone={c.tone} />
        </button>
      ))}
    </div>
  );
}
