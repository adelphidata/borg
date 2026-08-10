import { useState } from "react";
import { CheckIcon, MobileTitle, PageChrome, Pill, scrollCol, useIsMobile } from "@borg/ui";
import type { BorgClient, admin } from "@borg/client";
import { useMutation, useQuery } from "../../lib/useDaemon";
import { DaemonState } from "../../components/DaemonState";
import { formatDuration as fmtDuration, plural } from "../../lib/format";

interface HealthCheck {
  id: string;
  name: string;
  value: string;
  ok: boolean;
  /** Plain-language explanation revealed when the row is expanded. */
  detail: string;
  /** Optional lines rendered as a bullet list under the detail. */
  items?: string[];
}

/** Splits a `Category:message` warning into its two halves. */
export function splitWarning(warning: string): { label: string; text: string } {
  const colon = warning.indexOf(":");
  if (colon <= 0) return { label: "", text: warning.trim() };
  return { label: warning.slice(0, colon).trim(), text: warning.slice(colon + 1).trim() };
}

function fmtAgo(unixTs: number): string {
  if (unixTs === 0) return "never";
  const secs = Math.max(0, Date.now() / 1000 - unixTs);
  return `${fmtDuration(secs)} ago`;
}

/** Everything the check rows need, fetched together so one refetch renews it all. */
async function loadHealth(borg: BorgClient) {
  const [daemon, warnings, subprocesses, posture, heartbeat, tokens, cost] = await Promise.all([
    borg.admin.getDaemonStatus({}),
    borg.admin.getPersistentWarnings({}),
    borg.admin.getSubprocesses({}),
    borg.status.getPosture({}),
    borg.status.getHeartbeat({}),
    borg.admin.getMonthlyTokenTotal({}),
    borg.admin.getMonthlyTotalCost({}),
  ]);
  return { daemon, warnings, subprocesses, posture, heartbeat, tokens, cost };
}

type HealthData = Awaited<ReturnType<typeof loadHealth>>;

function buildChecks(d: HealthData): HealthCheck[] {
  const running = d.subprocesses.running.length;
  // The daemon sends "Reduced" / "Default" / "Boosted"; compare case-blind.
  const posture = (d.posture.posture || "default").toLowerCase();
  const heartbeatOn = d.heartbeat.nextInSeconds > 0;

  return [
    {
      id: "daemon",
      name: "Borg is running",
      value: `${fmtDuration(d.daemon.uptimeSecs)} without a restart`,
      ok: true,
      detail:
        `Version ${d.daemon.version}, up for ${fmtDuration(d.daemon.uptimeSecs)}. ` +
        (d.daemon.llmProvider
          ? `Thinking with ${d.daemon.llmModel || "the default model"} on ${d.daemon.llmProvider}.`
          : "No model provider is configured yet."),
    },
    {
      id: "warnings",
      name: "Warnings",
      value:
        d.warnings.warnings.length === 0 ? "None" : plural(d.warnings.warnings.length, "warning"),
      ok: d.warnings.warnings.length === 0,
      detail:
        d.warnings.warnings.length === 0
          ? "The last check-up found nothing that needs you."
          : "These checks need a look:",
      items: d.warnings.warnings.length === 0 ? undefined : d.warnings.warnings,
    },
    {
      id: "posture",
      name: "Security posture",
      value: posture.charAt(0).toUpperCase() + posture.slice(1),
      ok: posture !== "reduced",
      detail:
        posture === "reduced"
          ? "Borg is running with reduced protections (for example, secrets in a file instead of the keychain). It still works, but earns less trust."
          : `Protections are at their normal level. XP multiplier ×${d.posture.xpMultiplier.toFixed(2)}.`,
    },
    {
      id: "heartbeat",
      name: "Proactive check-ins",
      value: heartbeatOn ? `next in ${fmtDuration(d.heartbeat.nextInSeconds)}` : "off",
      ok: true,
      detail: heartbeatOn
        ? `Borg checks in on its own. Last one ${fmtAgo(d.heartbeat.lastUnixTs)}, next in ${fmtDuration(d.heartbeat.nextInSeconds)}.`
        : "Proactive check-ins are off, so Borg only acts when you ask.",
    },
    {
      id: "work",
      name: "Background work",
      value: `${plural(d.daemon.activeTasks, "schedule")} · ${plural(running, "process")}`,
      ok: true,
      detail:
        `${plural(d.daemon.activeTasks, "scheduled task")} active` +
        (d.daemon.pausedTasks > 0 ? `, ${d.daemon.pausedTasks} paused` : "") +
        `. ${plural(running, "subprocess")} running right now.`,
    },
    {
      id: "usage",
      name: "Usage this month",
      value:
        `${d.tokens.total.toLocaleString()} tokens` +
        (d.cost.present ? ` · $${d.cost.cost.toFixed(2)}` : ""),
      ok: true,
      detail: d.cost.present
        ? `Borg has used ${d.tokens.total.toLocaleString()} tokens this month, costing about $${d.cost.cost.toFixed(2)}.`
        : `Borg has used ${d.tokens.total.toLocaleString()} tokens this month. No price data yet, so there is no cost estimate.`,
    },
  ];
}

function summariseHeal(report: admin.HealReport): string {
  if (report.skipped) return "Nothing to tidy — the last sweep already ran recently.";
  const parts = [
    [report.prunedLogs, "log entries pruned"],
    [report.prunedActivity, "activity rows pruned"],
    [report.evictedEmbeddings, "embeddings evicted"],
    [report.healedTasks, "stalled tasks healed"],
    [report.messagesPurged, "old messages purged"],
    [report.wedgedRunsRecovered, "wedged runs recovered"],
    [report.backupsPruned, "backups pruned"],
  ].filter(([n]) => (n as number) > 0);
  return parts.length === 0
    ? "Tidy-up done. Nothing needed cleaning."
    : `Tidy-up done: ${parts.map(([n, what]) => `${n} ${what}`).join(", ")}.`;
}

export function HealthPage() {
  const isMobile = useIsMobile();
  const [openId, setOpenId] = useState<string | null>(null);
  const vitals = useQuery((borg) => borg.status.getVitals({}));
  const health = useQuery(loadHealth);
  const heal = useMutation((borg) => borg.admin.heal({}), {
    onSuccess: () => health.refetch(),
  });

  const refreshing = vitals.refreshing || health.refreshing;
  const lastChecked = refreshing ? "checking…" : "just now";
  const checks = health.state === "ready" ? buildChecks(health.data) : [];
  const failing = checks.filter((c) => !c.ok);
  const healReport = heal.error
    ? `Tidy-up failed: ${heal.error.message}`
    : heal.pending
      ? "Tidying up…"
      : heal.data
        ? summariseHeal(heal.data)
        : undefined;

  return (
    <PageChrome title="Health" meta={`last checked ${lastChecked}`}>
      <div className={scrollCol(isMobile, "px-[38px] py-10")}>
        <div className="flex max-w-[660px] flex-col gap-8">
          <MobileTitle>Health</MobileTitle>

          <div className="flex items-start gap-[22px]">
            <span
              className={`flex h-13 w-13 flex-none items-center justify-center rounded-full border-[1.5px] ${
                failing.length > 0 ? "border-alert" : "border-neon shadow-(--plateGlow)"
              }`}
            >
              <CheckIcon size={22} />
            </span>
            <div className="flex-1 pt-0.5">
              <h1 className="m-0 text-[27px] leading-[1.25] font-light tracking-[-0.025em] text-ink">
                {failing.length > 0 ? "Borg needs a look." : "Borg is healthy."}
              </h1>
              <p className="mt-[9px] mb-0 text-[14.5px] leading-[1.6] text-pretty text-ink2">
                {failing.length > 0 && <>Needs you: {failing.map((c) => c.name).join(", ")}. </>}
                Everything else checked out {lastChecked}.
              </p>
            </div>
          </div>

          <DaemonState query={vitals} label="vitals">
            {(v) => (
              <div className="flex flex-wrap gap-x-8 gap-y-3">
                {(
                  [
                    ["Stage", v.stage],
                    ["XP", v.xp],
                    ["Stability", v.stability],
                    ["Focus", v.focus],
                    ["Sync", v.sync],
                    ["Growth", v.growth],
                    ["Happiness", v.happiness],
                  ] as const
                ).map(([name, value]) => (
                  <div key={name}>
                    <div className="text-[9.5px] leading-none font-medium tracking-[0.16em] uppercase text-ink3">
                      {name}
                    </div>
                    <div className="mt-1.5 text-[15px] leading-none text-ink">{value}</div>
                  </div>
                ))}
              </div>
            )}
          </DaemonState>

          <DaemonState query={health} label="health checks">
            {() => (
              <div>
                {checks.map((c) => {
                  const open = openId === c.id;
                  return (
                    <div key={c.id}>
                      <div
                        className="flex cursor-pointer items-center gap-[15px] border-b border-line2 px-0.5 py-[15px]"
                        onClick={() => setOpenId((id) => (id === c.id ? null : c.id))}
                      >
                        <span
                          className={`h-[7px] w-[7px] flex-none rounded-full ${
                            c.ok ? "bg-neon" : "bg-alert"
                          }`}
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
                          {c.items && (
                            <ul className="m-0 flex list-none flex-col gap-[7px] p-0">
                              {c.items.map((item) => {
                                const { label, text } = splitWarning(item);
                                return (
                                  <li
                                    key={item}
                                    className="flex gap-[9px] text-[13px] leading-[1.5] text-ink2"
                                  >
                                    <span className="mt-[7px] h-[3px] w-[3px] flex-none rounded-full bg-ink3" />
                                    <span className="text-pretty">
                                      {label && (
                                        <span className="text-ink3">{label}&thinsp;—&thinsp;</span>
                                      )}
                                      {text}
                                    </span>
                                  </li>
                                );
                              })}
                            </ul>
                          )}
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            )}
          </DaemonState>

          {healReport && (
            <div className="text-[13px] leading-[1.6] text-pretty text-ink2">{healReport}</div>
          )}

          <div className="flex gap-[9px]">
            <Pill
              variant="primary"
              onClick={() => {
                vitals.refetch();
                health.refetch();
              }}
            >
              Check again
            </Pill>
            <Pill onClick={() => !heal.pending && void heal.run(undefined)}>
              {heal.pending ? "Tidying…" : "Tidy up now"}
            </Pill>
          </div>
        </div>
      </div>
    </PageChrome>
  );
}
