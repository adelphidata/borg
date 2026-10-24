import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Badge, Pill, PlusIcon, StatusDot, TrashIcon } from "@borg/ui";
import type { admin } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { PageGroup } from "../../components/PageGroup";
import { Switch } from "../../components/Switch";
import { useOptionalSettings } from "../../lib/settings";
import { showNotice } from "../../lib/notice";
import { badgeOf } from "../connections/useConnections";
import { NewSubscription } from "./NewSubscription";
import { ERROR_LINE } from "./SubscriptionFields";
import { hasEndpoint, toneOf, wordsOf, type Subscriptions } from "./useSubscriptions";

/** Starting points for an empty list: one per kind of source most people have. */
const EXAMPLES = [
  { preset: "sentry", label: "Sentry errors" },
  { preset: "slack", label: "Slack messages" },
  { preset: "postmark", label: "Email to Borg" },
];

export function subscriptionPath(id: string): string {
  return `/connections/subscriptions/${encodeURIComponent(id)}`;
}

function SubscriptionRow({
  sub,
  preset,
  onOpen,
  onToggle,
  onDelete,
}: {
  sub: admin.Subscription;
  preset: admin.SubscriptionPreset | undefined;
  onOpen: () => void;
  onToggle: (active: boolean) => void;
  onDelete: () => void;
}) {
  const words = wordsOf(sub, preset);
  return (
    <div className="group flex items-center gap-3 rounded-[10px] border border-line2 px-4 py-3">
      <StatusDot tone={toneOf(sub)} />
      <Badge on={sub.status === "active"}>{badgeOf(preset?.label ?? sub.provider)}</Badge>
      <button
        type="button"
        onClick={onOpen}
        className="block min-w-0 flex-1 cursor-pointer border-0 bg-transparent p-0 text-left"
      >
        <div className="truncate text-[13.5px] leading-[1.35] text-ink">{sub.name}</div>
        <div
          className={`mt-[3px] truncate text-[11.5px] leading-[1.4] ${
            toneOf(sub) === "alert" ? "text-alert" : "text-ink3"
          }`}
        >
          {words}
        </div>
      </button>
      {/* A storm is on in intent but held by Borg: the switch shows it off, and
          turning it on is Resume. */}
      <Switch on={sub.status === "active"} onChange={onToggle} label={`Enable ${sub.name}`} />
      <button
        type="button"
        aria-label={`Delete ${sub.name}`}
        onClick={onDelete}
        className="flex h-7 w-7 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-alert"
      >
        <TrashIcon size={14} />
      </button>
    </div>
  );
}

/** What wakes Borg from outside: one row per subscription. One section of Connections. */
export function SubscriptionsSection({ subs }: { subs: Subscriptions }) {
  const navigate = useNavigate();
  const settings = useOptionalSettings();
  const [adding, setAdding] = useState<{ preset?: string }>();
  const [confirm, setConfirm] = useState<string>();
  const { setActive, remove } = subs;
  const failure = (setActive.error ?? remove.error)?.message;
  const off =
    settings?.settings.state === "ready" &&
    settings.settings.data.get("subscriptions.enabled")?.value === "false";

  const toggle = (sub: admin.Subscription, active: boolean) => {
    remove.reset();
    void setActive.run({ id: sub.id, active }).then((stored) => {
      if (stored) showNotice(active ? `“${sub.name}” is on` : `Paused “${sub.name}”`);
    });
  };

  return (
    <PageGroup
      title="Subscriptions"
      action={
        !adding && (
          <Pill square onClick={() => setAdding({})}>
            <span className="flex items-center gap-1.5">
              <PlusIcon size={13} /> Add subscription
            </span>
          </Pill>
        )
      }
    >
      <DaemonState query={subs.query} label="your subscriptions">
        {(data) => (
          <>
            {off && (
              <div className="mb-3 flex items-center gap-3 text-[12.5px] leading-[1.45] text-ink2">
                <span className="min-w-0 flex-1">
                  Subscriptions are off, so no event wakes Borg.
                </span>
                <Pill
                  square
                  variant="primary"
                  onClick={() => void settings.save("subscriptions.enabled", "true")}
                >
                  Turn on
                </Pill>
              </div>
            )}

            {adding && (
              <NewSubscription
                presets={data.presets}
                subs={subs}
                initialPreset={adding.preset}
                onClose={() => setAdding(undefined)}
                onOpen={(id) => navigate(subscriptionPath(id))}
              />
            )}

            {failure && (
              <div role="alert" className={`mb-3 ${ERROR_LINE}`}>
                {failure}
              </div>
            )}

            <div className="flex flex-col gap-2">
              {data.subscriptions.length === 0 && !adding && (
                <div className="flex flex-col gap-3">
                  <div className="text-[13px] leading-[1.6] text-ink3">
                    Let an outside event wake Borg: an error alert, a message, an email.
                  </div>
                  <div className="flex flex-wrap gap-2">
                    {EXAMPLES.filter((e) => data.presets.some((p) => p.id === e.preset)).map(
                      (e) => (
                        <Pill key={e.preset} onClick={() => setAdding({ preset: e.preset })}>
                          {e.label}
                        </Pill>
                      ),
                    )}
                  </div>
                </div>
              )}
              {data.subscriptions.map((sub) =>
                confirm === sub.id ? (
                  <div
                    key={sub.id}
                    className="flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3"
                  >
                    <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
                      Remove {sub.name}?{" "}
                      {hasEndpoint(sub)
                        ? "Its address stops working at once."
                        : "Borg stops reacting to these events."}
                    </div>
                    <Pill square onClick={() => setConfirm(undefined)}>
                      Keep
                    </Pill>
                    <Pill
                      square
                      variant="danger"
                      onClick={() => {
                        setConfirm(undefined);
                        setActive.reset();
                        void remove.run(sub.id).then((r) => {
                          if (r !== undefined) showNotice(`Removed “${sub.name}”`);
                        });
                      }}
                    >
                      Remove
                    </Pill>
                  </div>
                ) : (
                  <SubscriptionRow
                    key={sub.id}
                    sub={sub}
                    preset={data.presets.find((p) => p.id === sub.provider)}
                    onOpen={() => navigate(subscriptionPath(sub.id))}
                    onToggle={(active) => toggle(sub, active)}
                    onDelete={() => setConfirm(sub.id)}
                  />
                ),
              )}
            </div>
          </>
        )}
      </DaemonState>
    </PageGroup>
  );
}
