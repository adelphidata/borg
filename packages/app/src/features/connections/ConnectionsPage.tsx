import { useNavigate } from "react-router-dom";
import {
  AlertBanner,
  Badge,
  MobileTitle,
  PageChrome,
  SECTION_LABEL,
  StatusDot,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { available, connected } from "../../data/mock";

/** Desktop: connected services as a card grid. */
function ConnCards({ open }: { open: (id: string) => void }) {
  return (
    <section>
      <div className={SECTION_LABEL}>Connected</div>
      <div className="grid grid-cols-4 gap-3">
        {connected.map((c) => (
          <div
            key={c.id}
            className="rounded-[11px] border border-line bg-surface2 p-4"
            onClick={() => open(c.id)}
          >
            <div className="mb-[11px] flex items-center justify-between">
              <Badge>{c.badge}</Badge>
              <StatusDot needsAttention={c.needsAttention} />
            </div>
            <div className="text-[13.5px] leading-[1.2] font-medium text-ink">{c.name}</div>
            <div className="mt-[5px] text-[11.5px] leading-[1.4] text-ink3">{c.sub}</div>
          </div>
        ))}
      </div>
    </section>
  );
}

/** Mobile: connected services as list rows. */
function ConnList({ open }: { open: (id: string) => void }) {
  return (
    <div className="flex flex-col">
      {connected.map((c) => (
        <div
          key={c.id}
          className="flex items-center gap-[13px] border-b border-line2 py-3.5"
          onClick={() => open(c.id)}
        >
          <Badge sizeClasses="h-[30px] w-[30px] rounded-[9px] text-[10.5px]">{c.badge}</Badge>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div className="text-[13.5px] leading-[1.25] font-medium text-ink">{c.name}</div>
            <div className="mt-[3px] text-[11.5px] leading-[1.3] text-ink3">{c.sub}</div>
          </div>
          <StatusDot needsAttention={c.needsAttention} />
        </div>
      ))}
    </div>
  );
}

function AddGrid({ open }: { open: (id: string) => void }) {
  const isMobile = useIsMobile();
  return (
    <section>
      <div className={SECTION_LABEL}>Add</div>
      <div className={`grid gap-3 ${isMobile ? "grid-cols-2" : "grid-cols-4"}`}>
        {available.map((c) => (
          <div
            key={c.id}
            className="flex cursor-pointer items-center gap-3 rounded-[11px] border border-line2 px-4 py-3.5 transition-[border-color] duration-150 hover:border-neon"
            onClick={() => open(c.id)}
          >
            <Badge on={false}>{c.badge}</Badge>
            <span className="text-[13px] leading-[1.2] font-medium text-ink2">{c.name}</span>
          </div>
        ))}
      </div>
    </section>
  );
}

export function ConnectionsPage() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const open = (id: string) => navigate(`/connections/${id}`);

  const attention = connected.filter((c) => c.needsAttention);
  const alerted = attention[0];
  const meta = `${connected.length} connected${
    attention.length > 0 ? ` · ${attention.length} needs attention` : ""
  }`;

  return (
    <PageChrome title="Connections" meta={meta}>
      <div className={scrollCol(isMobile, "px-[38px] py-[34px]")}>
        <div className="flex max-w-[860px] flex-col gap-[30px]">
          <MobileTitle>Connections</MobileTitle>
          {alerted && (
            <AlertBanner
              alert={alerted.alert}
              stacked={isMobile}
              onAction={() => open(alerted.id)}
            />
          )}
          {isMobile ? <ConnList open={open} /> : <ConnCards open={open} />}
          <AddGrid open={open} />
        </div>
      </div>
    </PageChrome>
  );
}
