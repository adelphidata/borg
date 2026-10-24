import { useCallback, useState } from "react";
import { Badge, CopyField, Pill, SECTION_LABEL, StepList, TEXT_FIELD } from "@borg/ui";
import type { admin } from "@borg/client";
import { Chevron, ROW_BUTTON } from "../../components/CategoryFold";
import { showNotice } from "../../lib/notice";
import { useMutation } from "../../lib/useDaemon";
import { badgeOf } from "../connections/useConnections";
import {
  Block,
  DeliveryPicker,
  ERROR_LINE,
  FilterEditor,
  HELP_LINE,
  Reachability,
  SigningSecretField,
} from "./SubscriptionFields";
import {
  changeOf,
  filterJsonOf,
  stepsWithKey,
  useSubscriptionWrites,
  useTunnel,
  type FilterDraft,
  type Subscriptions,
} from "./useSubscriptions";

/** Tile groups in the order the picker shows them; a group the daemon adds goes last. */
const GROUP_ORDER = ["Alerts", "Code", "Payments", "Messages", "Email", "Borg", "Custom"];

function groupsOf(presets: readonly admin.SubscriptionPreset[]) {
  const groups = new Map<string, admin.SubscriptionPreset[]>();
  for (const p of presets) groups.set(p.group, [...(groups.get(p.group) ?? []), p]);
  const rank = (g: string) => (GROUP_ORDER.includes(g) ? GROUP_ORDER.indexOf(g) : 99);
  return [...groups.entries()].sort(([a], [b]) => rank(a) - rank(b));
}

/** Step 1: one tile per source, grouped. */
function SourcePicker({
  presets,
  onPick,
}: {
  presets: readonly admin.SubscriptionPreset[];
  onPick: (preset: admin.SubscriptionPreset) => void;
}) {
  return (
    <div className="flex flex-col gap-4">
      {groupsOf(presets).map(([group, items]) => (
        <div key={group}>
          <div className={SECTION_LABEL}>{group}</div>
          <div className="grid grid-cols-[repeat(auto-fill,minmax(180px,1fr))] gap-2">
            {items.map((p) => (
              <button
                key={p.id}
                type="button"
                onClick={() => onPick(p)}
                className="flex cursor-pointer items-start gap-2.5 rounded-[10px] border border-line2 bg-surface px-3 py-2.5 text-left hover:border-neon"
              >
                <span aria-hidden="true">
                  <Badge on={false}>{badgeOf(p.label)}</Badge>
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block text-[13px] leading-[1.3] font-medium text-ink">
                    {p.label}
                  </span>
                  <span className="mt-[2px] block text-[11.5px] leading-[1.35] text-ink3">
                    {p.blurb}
                  </span>
                </span>
              </button>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}

/** Steps 2 and 3: when it wakes Borg, and what Borg does then. */
function Configure({
  preset,
  subs,
  onBack,
  onCancel,
  onCreated,
}: {
  preset: admin.SubscriptionPreset;
  subs: Subscriptions;
  onBack: () => void;
  onCancel: () => void;
  onCreated: (created: admin.SubscriptionCreated) => void;
}) {
  const [filter, setFilter] = useState<FilterDraft>({});
  const [instruction, setInstruction] = useState(preset.defaultInstruction);
  const [delivery, setDelivery] = useState({ channel: "", target: "" });
  const [name, setName] = useState(preset.defaultName);
  const [advanced, setAdvanced] = useState(false);
  const { create } = subs;

  const submit = () => {
    void create
      .run({
        provider: preset.id,
        // Empty takes the preset's own, so an untouched field stays its default.
        name: name.trim() === preset.defaultName ? "" : name.trim(),
        instruction: instruction.trim(),
        filterJson: filterJsonOf(filter, preset.filterFields),
        deliveryChannel: delivery.channel,
        deliveryTarget: delivery.target.trim(),
        signingSecret: "",
        rateLimitPerHour: 0,
        timeoutSecs: 0,
        // Paused until the sender is set up: an endpoint nobody has wired yet
        // has nothing to hear, and a channel source starts on Enable.
        active: false,
      })
      .then((created) => {
        if (!created) return;
        showNotice(`Created “${created.subscription?.name ?? preset.defaultName}”`);
        onCreated(created);
      });
  };

  return (
    <div className="flex flex-col gap-5">
      <div className="flex items-center gap-3">
        <Badge>{badgeOf(preset.label)}</Badge>
        <div className="min-w-0 flex-1">
          <div className="text-[13.5px] leading-[1.3] font-medium text-ink">{preset.label}</div>
          <div className="text-[11.5px] leading-[1.35] text-ink3">{preset.blurb}</div>
        </div>
        <Pill square onClick={onBack}>
          Change
        </Pill>
      </div>

      {preset.filterFields.length > 0 && (
        <Block label="When">
          <FilterEditor preset={preset} draft={filter} onChange={setFilter} />
        </Block>
      )}

      <Block label="Do">
        <div className="flex flex-col gap-3">
          <textarea
            aria-label="Instruction"
            rows={4}
            className={`${TEXT_FIELD} w-full resize-y leading-[1.5]`}
            placeholder={preset.defaultInstruction}
            value={instruction}
            onChange={(e) => setInstruction(e.target.value)}
          />
          <DeliveryPicker
            source={preset.source}
            channels={subs.query.state === "ready" ? subs.query.data.deliveryChannels : []}
            channel={delivery.channel}
            target={delivery.target}
            onChange={setDelivery}
          />
        </div>
      </Block>

      <div>
        <button
          type="button"
          aria-expanded={advanced}
          className={`${ROW_BUTTON} gap-2 p-0 text-[12px] leading-none text-ink2 hover:text-ink`}
          onClick={() => setAdvanced((a) => !a)}
        >
          <Chevron open={advanced} />
          Advanced
        </button>
        {advanced && (
          <label className="mt-3 flex flex-col gap-2">
            <span className="text-[12px] leading-none text-ink2">Name</span>
            <input
              className={`${TEXT_FIELD} w-full`}
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </label>
        )}
      </div>

      {create.error && (
        <div role="alert" className={ERROR_LINE}>
          {create.error.message}
        </div>
      )}
      <div className="flex justify-end gap-2">
        <Pill square onClick={onCancel}>
          Cancel
        </Pill>
        <Pill square variant="primary" disabled={create.pending} onClick={submit}>
          {create.pending ? "Creating…" : "Create"}
        </Pill>
      </div>
    </div>
  );
}

/**
 * After Create: everything the sender needs, in place, and proof it works.
 *
 * The line under the steps listens to the activity stream, so the moment the
 * provider's first event lands the user sees it here — before they enable
 * anything, since a paused subscription still reports what reached it.
 */
export function ConnectCard({
  created,
  preset,
  subs,
  onClose,
  onOpen,
}: {
  created: admin.SubscriptionCreated;
  preset: admin.SubscriptionPreset | undefined;
  subs: Subscriptions;
  onClose: () => void;
  onOpen: () => void;
}) {
  const sub = created.subscription!;
  const label = preset?.label ?? sub.provider;
  const [endpoint, setEndpoint] = useState(sub.endpoint);
  const [steps, setSteps] = useState(created.senderSteps);
  const placeholder = subs.query.state === "ready" ? subs.query.data.keyPlaceholder : "";
  const [secretSaved, setSecretSaved] = useState(sub.hasSigningSecret);
  const [tested, setTested] = useState<number>();
  const heard = subs.activity[sub.id] ?? { received: 0, finished: 0 };

  // The endpoint is worked out when it is read, so a tunnel that has just come
  // up shows on the next read. Those steps mask the key; the one in hand goes back in.
  const reread = useMutation((borg) => borg.admin.getSubscription({ id: sub.id }));
  const { run: rereadRun } = reread;
  const refreshEndpoint = useCallback(() => {
    void rereadRun(undefined).then((detail) => {
      if (!detail?.subscription) return;
      setEndpoint(detail.subscription.endpoint);
      setSteps(stepsWithKey(detail.senderSteps, created.key, placeholder));
    });
  }, [rereadRun, created.key, placeholder]);
  const tunnel = useTunnel(refreshEndpoint);

  // Its own writes, so a refusal here is said here and not again on the
  // row list behind it.
  const { setActive, update, test } = useSubscriptionWrites(subs.query.refetch);
  const enable = () => {
    void setActive.run({ id: sub.id, active: true }).then((stored) => {
      if (!stored) return;
      showNotice(`“${sub.name}” is on`);
      onClose();
    });
  };

  const url = endpoint?.url ?? "";
  const waiting = !!preset?.takesSigningSecret && !secretSaved;
  const testRunning = tested !== undefined && heard.finished <= tested;

  return (
    <div className="flex flex-col gap-5">
      <div className="flex items-center gap-3">
        <Badge>{badgeOf(label)}</Badge>
        <div className="min-w-0 flex-1">
          <div className="text-[13.5px] leading-[1.3] font-medium text-ink">{sub.name}</div>
          <div className="text-[11.5px] leading-[1.35] text-ink3">
            {url === ""
              ? "Ready. Enable when you want Borg to start reacting."
              : `Copy these into ${label}.`}
          </div>
        </div>
      </div>

      {url !== "" && (
        <>
          <CopyField label="URL" value={url} />
          {created.key !== "" && <CopyField label="Key" value={created.key} secret />}
          <Reachability endpoint={endpoint} tunnel={tunnel} />
          {steps.length > 0 && (
            <Block label={`In ${label}`}>
              <StepList steps={steps} label={`Set up ${label}`} />
            </Block>
          )}
          {preset?.takesSigningSecret && (
            <SigningSecretField
              provider={label}
              saved={secretSaved}
              pending={update.pending}
              error={update.error?.message}
              onSave={(secret) =>
                update
                  .run(changeOf(sub.id, { setSigningSecret: true, signingSecret: secret }))
                  .then((stored) => {
                    if (stored) setSecretSaved(true);
                    return stored !== undefined;
                  })
              }
            />
          )}
          <div role="status" className="flex items-center gap-2 text-[12.5px] leading-[1.4]">
            {heard.received > 0 ? (
              <span className="text-ink">Got it. First event received just now.</span>
            ) : (
              <span className="text-ink3">Listening for the first event…</span>
            )}
          </div>
        </>
      )}

      {tested !== undefined && (
        <div className={HELP_LINE}>
          {testRunning ? "Test sent. Borg is working on it…" : "Test finished. "}
          {!testRunning && (
            <button
              type="button"
              className="cursor-pointer border-0 bg-transparent p-0 text-[11.5px] text-neon-ink underline"
              onClick={onOpen}
            >
              See the run
            </button>
          )}
        </div>
      )}

      {waiting && <div className={HELP_LINE}>Enable unlocks once the signing secret is saved.</div>}
      {(setActive.error || test.error) && (
        <div role="alert" className={ERROR_LINE}>
          {(setActive.error ?? test.error)?.message}
        </div>
      )}
      <div className="flex flex-wrap justify-end gap-2">
        <Pill square onClick={onClose}>
          Keep it paused
        </Pill>
        <Pill
          square
          disabled={test.pending || testRunning}
          onClick={() => {
            const before = heard.finished;
            void test.run(sub.id).then((event) => event && setTested(before));
          }}
        >
          {test.pending || testRunning ? "Running…" : "Send a test event"}
        </Pill>
        <Pill square variant="primary" disabled={setActive.pending || waiting} onClick={enable}>
          {setActive.pending ? "Enabling…" : "Enable"}
        </Pill>
      </div>
    </div>
  );
}

/**
 * The add flow, inline on the Connections page: pick a source, say when and
 * what, Create — then the same card becomes the Connect card.
 */
export function NewSubscription({
  presets,
  subs,
  initialPreset,
  onClose,
  onOpen,
}: {
  presets: readonly admin.SubscriptionPreset[];
  subs: Subscriptions;
  /** Opens straight on this source (an example chip). */
  initialPreset?: string;
  onClose: () => void;
  /** Open the detail page of a subscription just made. */
  onOpen: (id: string) => void;
}) {
  const [preset, setPreset] = useState(() => presets.find((p) => p.id === initialPreset));
  const [created, setCreated] = useState<admin.SubscriptionCreated>();
  const close = () => {
    subs.create.reset();
    onClose();
  };

  return (
    <div className="mb-4 flex flex-col gap-4 rounded-[10px] border border-line bg-surface2 p-4">
      {created ? (
        <ConnectCard
          created={created}
          preset={preset}
          subs={subs}
          onClose={close}
          onOpen={() => created.subscription && onOpen(created.subscription.id)}
        />
      ) : preset ? (
        <Configure
          // A new source starts from its own defaults, not the last one's.
          key={preset.id}
          preset={preset}
          subs={subs}
          onBack={() => setPreset(undefined)}
          onCancel={close}
          onCreated={setCreated}
        />
      ) : (
        <>
          <div className="text-[12.5px] leading-[1.45] text-ink2">
            What should wake Borg? Pick where the events come from.
          </div>
          <SourcePicker presets={presets} onPick={setPreset} />
          <div className="flex justify-end">
            <Pill square onClick={close}>
              Cancel
            </Pill>
          </div>
        </>
      )}
    </div>
  );
}
