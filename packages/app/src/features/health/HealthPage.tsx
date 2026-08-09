import { useState } from "react";
import { CheckIcon, MobileTitle, PageChrome, Pill, scrollCol, useIsMobile } from "@borg/ui";
import { checks } from "../../data/mock";

export function HealthPage() {
  const isMobile = useIsMobile();
  const [openId, setOpenId] = useState<string | null>(null);
  // No daemon yet — "Check again" just refreshes the mock timestamp.
  const [lastChecked, setLastChecked] = useState("a few minutes ago");
  const failing = checks.filter((c) => !c.ok);

  return (
    <PageChrome title="Health" meta={`last checked ${lastChecked}`}>
      <div className={scrollCol(isMobile, "px-[38px] py-10")}>
        <div className="flex max-w-[660px] flex-col gap-8">
          <MobileTitle>Health</MobileTitle>

          <div className="flex items-start gap-[22px]">
            <span className="flex h-13 w-13 flex-none items-center justify-center rounded-full border-[1.5px] border-neon shadow-(--plateGlow)">
              <CheckIcon size={22} />
            </span>
            <div style={{ flex: 1, paddingTop: 2 }}>
              <h1 className="m-0 text-[27px] leading-[1.25] font-light tracking-[-0.025em] text-ink">
                Borg is healthy.
              </h1>
              <p className="mt-[9px] mb-0 text-[14.5px] leading-[1.6] text-pretty text-ink2">
                {failing.length > 0 && <>One thing needs you: the {failing[0].name}. </>}
                Everything else checked out {lastChecked}.
              </p>
            </div>
          </div>

          <div>
            {checks.map((c) => {
              const open = openId === c.id;
              return (
                <div key={c.id}>
                  <div
                    className="flex items-center gap-[15px] border-b border-line2 px-0.5 py-[15px]"
                    onClick={() => setOpenId((id) => (id === c.id ? null : c.id))}
                  >
                    <span
                      className="h-[7px] w-[7px] flex-none rounded-full"
                      style={{
                        background: c.ok ? "var(--neon)" : "var(--alert)",
                      }}
                    />
                    <span className="flex-1 text-[14px] leading-[1.35] text-ink">{c.name}</span>
                    <span className="text-right text-[12.5px] leading-[1.35] text-ink3">
                      {c.value}
                    </span>
                  </div>
                  {open && (
                    <div className="flex flex-col items-start gap-3 border-b border-line2 pt-1 pr-0.5 pb-4 pl-[22px]">
                      <p className="mt-2.5 mb-0 text-[13px] leading-[1.6] text-pretty text-ink2">
                        {c.detail}
                      </p>
                      {c.action && <Pill variant="danger">{c.action}</Pill>}
                    </div>
                  )}
                </div>
              );
            })}
          </div>

          <div className="flex gap-[9px]">
            <Pill variant="primary" onClick={() => setLastChecked("just now")}>
              Check again
            </Pill>
            <Pill>Tidy up now</Pill>
          </div>
        </div>
      </div>
    </PageChrome>
  );
}
