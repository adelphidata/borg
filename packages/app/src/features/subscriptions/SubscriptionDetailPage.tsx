import { useState, type ReactNode } from "react";
import { useNavigate, useParams } from "react-router-dom";
import {
  AlertBanner,
  BackLink,
  Badge,
  CodeBlock,
  CopyField,
  DETAIL_COL,
  MobileTitle,
  PageChrome,
  Pill,
  SECTION_LABEL,
  StatusDot,
  StepList,
  TEXT_FIELD,
  scrollCol,
  useIsMobile,
  type DotTone,
} from "@borg/ui";
import { GrpcStatus, type admin } from "@borg/client";
import { Chevron, ROW_BUTTON } from "../../components/CategoryFold";
import { DaemonState } from "../../components/DaemonState";
import { formatElapsed } from "../../lib/format";
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
  ago,
  changeOf,
  filterDraftOf,
  filterJsonOf,
  hasEndpoint,
  SETTLED_EVENT,
  stepsWithoutKey,
  toneOf,
  usePollWhile,
  useSubscription,
  useTunnel,
  wordsOf,
  type FilterDraft,
  type SubscriptionView,
} from "./useSubscriptions";

const EVENT_WORDS: Record<string, string> = {
  pending: "Waiting",
  running: "Running",
  succeeded: "Done",
  failed: "Failed",
  skipped: "Skipped",
  silent: "Nothing to report",
};

const EVENT_TONE: Record<string, DotTone> = {
  pending: "idle",
  running: "busy",
  succeeded: "ok",
  failed: "alert",
  skipped: "paused",
  silent: "ok",
};

/** Payloads shown per run when it is opened; a digest of 37 shows the first few. */
const PAYLOADS_SHOWN = 3;

/**
 * Events grouped into the runs that handled them, newest first. Events still
 * waiting have no run yet and stand alone.
 */
export function runsOf(events: readonly admin.SubscriptionEvent[]): admin.SubscriptionEvent[][] {
  const runs = new Map<string, admin.SubscriptionEvent[]>();
  for (const e of events) {
    const key = e.runId || `event:${e.id}`;
    runs.set(key, [...(runs.get(key) ?? []), e]);
  }
  return [...runs.values()];
}

function pretty(payload: string): string {
  try {
    return JSON.stringify(JSON.parse(payload), null, 2);
  } catch {
    return payload;
  }
}

/**
 * A stored value being edited. A refetch that lands while nothing is typed
 * brings the stored value in; typed changes are never thrown away.
 */
function useDraft<T>(stored: T) {
  const key = JSON.stringify(stored);
  const [state, setState] = useState({ draft: stored, dirty: false, key });
  if (!state.dirty && state.key !== key) setState({ draft: stored, dirty: false, key });
  return {
    draft: state.draft,
    dirty: state.dirty,
    set: (draft: T) => setState((s) => ({ ...s, draft, dirty: true })),
    discard: () => setState({ draft: stored, dirty: false, key }),
    /** Saved: keep what was typed until the re-read brings the stored copy. */
    settle: () => setState((s) => ({ ...s, dirty: false })),
  };
}

/** A section with a Save that writes one change and says why it was refused. */
function Editable({
  label,
  dirty,
  saving,
  error,
  invalid,
  onSave,
  onDiscard,
  children,
}: {
  label: string;
  dirty: boolean;
  saving: boolean;
  error: string | undefined;
  /** Why the typed value cannot be saved; Save stays off while it is set. */
  invalid?: string;
  onSave: () => void;
  onDiscard: () => void;
  children: ReactNode;
}) {
  return (
    <Block label={label}>
      <div className="flex flex-col gap-3">
        {children}
        {error && (
          <div role="alert" className={ERROR_LINE}>
            {error}
          </div>
        )}
        {dirty && invalid && <div className={HELP_LINE}>{invalid}</div>}
        {dirty && (
          <div className="flex gap-2">
            <Pill
              square
              variant="primary"
              disabled={saving || invalid !== undefined}
              onClick={onSave}
              aria-label={`Save ${label.toLowerCase()}`}
            >
              {saving ? "Saving…" : "Save"}
            </Pill>
            <Pill square onClick={onDiscard} aria-label={`Discard ${label.toLowerCase()}`}>
              Discard
            </Pill>
          </div>
        )}
      </div>
    </Block>
  );
}

function RunRow({ run }: { run: admin.SubscriptionEvent[] }) {
  const [open, setOpen] = useState(false);
  const lead = run[0]!;
  const words = EVENT_WORDS[lead.status] ?? lead.status;
  const meta = [
    run.length > 1 ? `${run.length} events → 1 run` : undefined,
    words,
    lead.receivedAtUnix > 0 ? ago(lead.receivedAtUnix) : undefined,
    lead.durationMs > 0 ? formatElapsed(lead.durationMs) : undefined,
  ].filter(Boolean);
  return (
    <div className="rounded-[10px] border border-line2">
      <button
        type="button"
        aria-expanded={open}
        className={`${ROW_BUTTON} gap-3 px-4 py-3`}
        onClick={() => setOpen((o) => !o)}
      >
        <StatusDot tone={EVENT_TONE[lead.status] ?? "idle"} />
        <span className="min-w-0 flex-1">
          <span className="flex items-center gap-2">
            {lead.kind === "test" && (
              <span className="flex-none rounded-full bg-surface3 px-2 py-[3px] text-[10px] leading-none font-medium text-ink2">
                Test
              </span>
            )}
            <span className="truncate text-[13px] leading-[1.35] text-ink">
              {lead.summary || lead.kind}
            </span>
          </span>
          <span className="mt-[3px] block text-[11.5px] leading-[1.4] text-ink3">
            {meta.join(" · ")}
          </span>
          {lead.status === "failed" && lead.error && (
            <span className="mt-[3px] block text-[11.5px] leading-[1.4] text-alert">
              {lead.error}
            </span>
          )}
        </span>
        <Chevron open={open} />
      </button>
      {open && (
        <div className="flex flex-col gap-2 border-t border-line2 px-4 py-3">
          {lead.result && (
            <div className="text-[12.5px] leading-[1.5] whitespace-pre-wrap text-ink2">
              {lead.result}
            </div>
          )}
          {run.slice(0, PAYLOADS_SHOWN).map((e) => (
            <div key={e.id}>
              <CodeBlock code={pretty(e.payload)} lang="json" />
              {e.payloadTruncated && (
                <div className={HELP_LINE}>Cut short for display; Borg saw all of it.</div>
              )}
            </div>
          ))}
          {run.length > PAYLOADS_SHOWN && (
            <div className={HELP_LINE}>And {run.length - PAYLOADS_SHOWN} more in this run.</div>
          )}
        </div>
      )}
    </div>
  );
}

function EndpointSection({
  sub,
  preset,
  steps,
  placeholder,
  view,
  secret,
}: {
  sub: admin.Subscription;
  preset: admin.SubscriptionPreset | undefined;
  /** As the daemon reads them: the key masked. */
  steps: string[];
  placeholder: string;
  view: SubscriptionView;
  /** The signing secret's own save, so another section's refusal is not said here. */
  secret: {
    pending: boolean;
    error: string | undefined;
    save: (secret: string) => Promise<boolean>;
  };
}) {
  const label = preset?.label ?? sub.provider;
  const [shown, setShown] = useState<admin.SubscriptionKey>();
  // One Show for the key and the steps that carry it.
  const [keyShown, setKeyShown] = useState(false);
  const [confirmRotate, setConfirmRotate] = useState(false);
  const [howTo, setHowTo] = useState(false);
  const tunnel = useTunnel(view.query.refetch);
  const { reveal, rotate } = view;
  const currentSteps = !shown
    ? steps
    : keyShown
      ? shown.senderSteps
      : stepsWithoutKey(shown.senderSteps, shown.key, placeholder);
  const keyError = (reveal.error ?? rotate.error)?.message;

  const replace = (
    <Pill square onClick={() => setConfirmRotate(true)}>
      Replace key
    </Pill>
  );

  return (
    <Block label="Endpoint">
      <div className="flex flex-col gap-4">
        <CopyField label="URL" value={sub.endpoint?.url ?? ""} />
        {shown ? (
          <CopyField
            label="Key"
            value={shown.key}
            secret
            shown={keyShown}
            onShownChange={setKeyShown}
            actions={replace}
          />
        ) : (
          <div>
            <div className={SECTION_LABEL}>Key</div>
            <div className="flex flex-wrap items-center gap-2">
              <span className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink3">
                Hidden. Reveal it to copy it again.
              </span>
              <Pill
                square
                disabled={reveal.pending}
                onClick={() =>
                  void reveal.run(sub.id).then((k) => {
                    if (!k) return;
                    // Asked for in so many words: show it, and Hide covers both.
                    setShown(k);
                    setKeyShown(true);
                  })
                }
              >
                {reveal.pending ? "Revealing…" : "Reveal key"}
              </Pill>
              {replace}
            </div>
          </div>
        )}
        {confirmRotate && (
          <div className="flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3">
            <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
              Replace the key? The old one stops working at once, and {label} needs the new one.
            </div>
            <Pill square onClick={() => setConfirmRotate(false)}>
              Keep
            </Pill>
            <Pill
              square
              variant="danger"
              onClick={() => {
                setConfirmRotate(false);
                void rotate.run(sub.id).then((k) => {
                  if (!k) return;
                  setShown(k);
                  setKeyShown(false);
                  setHowTo(true);
                  showNotice(`Key replaced. Paste the new one into ${label}.`);
                });
              }}
            >
              Replace
            </Pill>
          </div>
        )}
        {keyError && (
          <div role="alert" className={ERROR_LINE}>
            {keyError}
          </div>
        )}
        <Reachability endpoint={sub.endpoint} tunnel={tunnel} />
        {currentSteps.length > 0 && (
          <div>
            <button
              type="button"
              aria-expanded={howTo}
              className={`${ROW_BUTTON} gap-2 p-0 text-[12px] leading-none text-ink2 hover:text-ink`}
              onClick={() => setHowTo((o) => !o)}
            >
              <Chevron open={howTo} />
              How to set it up in {label}
            </button>
            {howTo && (
              <div className="mt-3">
                <StepList steps={currentSteps} label={`Set up ${label}`} />
              </div>
            )}
          </div>
        )}
        {preset?.takesSigningSecret && (
          <SigningSecretField
            provider={label}
            saved={sub.hasSigningSecret}
            pending={secret.pending}
            error={secret.error}
            onSave={secret.save}
          />
        )}
      </div>
    </Block>
  );
}

/**
 * A typed count, or undefined for anything that is not a whole number above
 * zero. The daemon reads 0 as "leave it alone", so a 0 must never be sent as
 * if it were a value.
 */
function countOf(text: string): number | undefined {
  const t = text.trim();
  if (!/^[0-9]+$/.test(t)) return undefined;
  const n = Number.parseInt(t, 10);
  return n > 0 ? n : undefined;
}

function Advanced({
  sub,
  runsToday,
  saving,
  error,
  onSave,
  nameSaving,
  nameError,
  onSaveName,
}: {
  sub: admin.Subscription;
  runsToday: number;
  saving: boolean;
  error: string | undefined;
  onSave: (change: admin.SubscriptionChange, settle: () => void) => void;
  nameSaving: boolean;
  nameError: string | undefined;
  onSaveName: (change: admin.SubscriptionChange, settle: () => void) => void;
}) {
  const [open, setOpen] = useState(false);
  const name = useDraft(sub.name);
  const limits = useDraft({
    rate: String(sub.rateLimitPerHour),
    timeout: String(sub.timeoutSecs),
  });
  const rate = countOf(limits.draft.rate);
  const timeout = countOf(limits.draft.timeout);
  const limitsInvalid = rate === undefined || timeout === undefined;
  const save = () => {
    if (rate === undefined || timeout === undefined) return;
    onSave(changeOf(sub.id, { rateLimitPerHour: rate, timeoutSecs: timeout }), limits.settle);
  };
  return (
    <section>
      <button
        type="button"
        aria-expanded={open}
        className={`${ROW_BUTTON} gap-2 p-0 text-[12px] leading-none text-ink2 hover:text-ink`}
        onClick={() => setOpen((o) => !o)}
      >
        <Chevron open={open} />
        Advanced
      </button>
      {open && (
        <div className="mt-4 flex flex-col gap-4">
          {/* Here as well as the page title: a phone has no title bar to rename from. */}
          <Editable
            label="Name"
            dirty={name.dirty}
            saving={nameSaving}
            error={nameError}
            invalid={name.draft.trim() === "" ? "A name cannot be empty." : undefined}
            onDiscard={name.discard}
            onSave={() => onSaveName(changeOf(sub.id, { name: name.draft.trim() }), name.settle)}
          >
            <input
              aria-label="Name"
              className={`${TEXT_FIELD} w-full`}
              value={name.draft}
              onChange={(e) => name.set(e.target.value)}
            />
          </Editable>
          <div className="grid grid-cols-2 gap-3">
            <label className="flex flex-col gap-2">
              <span className="text-[12px] leading-none text-ink2">Runs per hour, at most</span>
              <input
                type="number"
                min={1}
                className={`${TEXT_FIELD} w-full`}
                value={limits.draft.rate}
                onChange={(e) => limits.set({ ...limits.draft, rate: e.target.value })}
              />
            </label>
            <label className="flex flex-col gap-2">
              <span className="text-[12px] leading-none text-ink2">Stop a run after (seconds)</span>
              <input
                type="number"
                min={10}
                className={`${TEXT_FIELD} w-full`}
                value={limits.draft.timeout}
                onChange={(e) => limits.set({ ...limits.draft, timeout: e.target.value })}
              />
            </label>
          </div>
          {error && (
            <div role="alert" className={ERROR_LINE}>
              {error}
            </div>
          )}
          {limits.dirty && limitsInvalid && (
            <div className={HELP_LINE}>Use whole numbers above zero.</div>
          )}
          {limits.dirty && (
            <div className="flex gap-2">
              <Pill square variant="primary" disabled={saving || limitsInvalid} onClick={save}>
                {saving ? "Saving…" : "Save limits"}
              </Pill>
              <Pill square onClick={limits.discard} aria-label="Discard limits">
                Discard
              </Pill>
            </div>
          )}
          <dl className="m-0 grid grid-cols-2 gap-x-4 gap-y-2 text-[12.5px] leading-[1.4]">
            <dt className="text-ink3">Runs today</dt>
            <dd className="m-0 text-ink tabular-nums">{runsToday}</dd>
            <dt className="text-ink3">Events dropped in bursts or while paused</dt>
            <dd className="m-0 text-ink tabular-nums">{sub.droppedCount}</dd>
          </dl>
        </div>
      )}
    </section>
  );
}

function Detail({
  detail,
  sub,
  preset,
  channels,
  placeholder,
  view,
  renameError,
}: {
  detail: admin.SubscriptionDetail;
  sub: admin.Subscription;
  preset: admin.SubscriptionPreset | undefined;
  /** Delivery channels set up on this machine. */
  channels: readonly string[];
  /** What the daemon writes in the steps in place of the key. */
  placeholder: string;
  view: SubscriptionView;
  /** Why the last rename from the page title was refused. */
  renameError: string | undefined;
}) {
  const navigate = useNavigate();
  const { setActive, update, test, remove } = view;
  const [editing, setEditing] = useState<string>();
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [testRun, setTestRun] = useState<{ id: string; finishedBefore: number }>();

  const instruction = useDraft(sub.instruction);
  const filter = useDraft<FilterDraft>(filterDraftOf(sub.filterJson));
  const delivery = useDraft({ channel: sub.deliveryChannel, target: sub.deliveryTarget });

  const write = (section: string, change: admin.SubscriptionChange, settle: () => void) => {
    setEditing(section);
    return update.run(change).then((stored) => {
      if (stored) settle();
      return stored !== undefined;
    });
  };
  const sectionError = (section: string) =>
    editing === section ? update.error?.message : undefined;

  const resume = (active: boolean) => {
    void setActive.run({ id: sub.id, active }).then((stored) => {
      if (stored) showNotice(active ? `“${sub.name}” is on` : `Paused “${sub.name}”`);
    });
  };

  const testEvent = testRun && detail.events.find((e) => e.id === testRun.id);
  const testing =
    test.pending ||
    (testRun !== undefined &&
      view.activity.finished <= testRun.finishedBefore &&
      !(testEvent && SETTLED_EVENT.has(testEvent.status)));
  // The finished frame normally ends the wait; a re-read ends it if the
  // stream dropped that frame.
  usePollWhile(testing && !test.pending, view.query.refetch);
  const runs = runsOf(detail.events);
  const label = preset?.label ?? sub.provider;
  const headerError = renameError ?? (setActive.error ?? test.error ?? remove.error)?.message;

  return (
    <>
      <div className="flex items-center gap-4">
        <Badge
          sizeClasses="h-[34px] w-[34px] rounded-[10px] text-[11px]"
          on={sub.status === "active"}
        >
          {badgeOf(label)}
        </Badge>
        <div className="min-w-0 flex-1">
          <MobileTitle>{sub.name}</MobileTitle>
          <div className="flex items-center gap-2 text-[12.5px] leading-[1.4] text-ink2">
            <StatusDot tone={toneOf(sub)} />
            <span>{wordsOf(sub, preset)}</span>
          </div>
          <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">{label}</div>
        </div>
      </div>

      {confirmDelete ? (
        <div className="flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3">
          <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
            Delete {sub.name}?{" "}
            {hasEndpoint(sub)
              ? "Its address stops working at once."
              : "Borg stops reacting to these events."}
          </div>
          <Pill square onClick={() => setConfirmDelete(false)}>
            Keep
          </Pill>
          <Pill
            square
            variant="danger"
            onClick={() =>
              void remove.run(sub.id).then((r) => {
                if (r === undefined) return;
                showNotice(`Removed “${sub.name}”`);
                navigate("/connections");
              })
            }
          >
            Delete
          </Pill>
        </div>
      ) : (
        <div className="flex flex-wrap items-center gap-2">
          {sub.status === "active" ? (
            <Pill square disabled={setActive.pending} onClick={() => resume(false)}>
              Pause
            </Pill>
          ) : (
            <Pill
              square
              variant="primary"
              disabled={setActive.pending}
              onClick={() => resume(true)}
            >
              {sub.status === "storm" ? "Resume" : "Enable"}
            </Pill>
          )}
          <Pill
            square
            disabled={testing}
            onClick={() => {
              const finishedBefore = view.activity.finished;
              void test.run(sub.id).then((e) => e && setTestRun({ id: e.id, finishedBefore }));
            }}
          >
            {testing ? "Running…" : "Send a test event"}
          </Pill>
          {runs.length > 0 && (
            <Pill square onClick={() => navigate(`/c/${encodeURIComponent(sub.sessionKey)}`)}>
              Open conversation
            </Pill>
          )}
          <span className="flex-1" />
          <Pill square variant="danger" onClick={() => setConfirmDelete(true)}>
            Delete
          </Pill>
        </div>
      )}
      {headerError && (
        <div role="alert" className={ERROR_LINE}>
          {headerError}
        </div>
      )}

      {sub.status === "storm" && (
        <AlertBanner
          alert={{
            title:
              sub.lastRunStatus === "failed"
                ? "Paused itself: its runs kept failing"
                : "Paused itself: too many events at once",
            stackedTitle: "Paused itself",
            sub: "Borg stopped this subscription to save model usage. Narrow what it listens for, then resume it.",
            action: "Resume",
          }}
          onAction={() => resume(true)}
        />
      )}

      <Editable
        label="Instruction"
        dirty={instruction.dirty}
        saving={update.pending && editing === "instruction"}
        error={sectionError("instruction")}
        invalid={
          instruction.draft.trim() === ""
            ? "An instruction cannot be empty. Discard to keep the one it has."
            : undefined
        }
        onDiscard={instruction.discard}
        onSave={() =>
          void write(
            "instruction",
            changeOf(sub.id, { instruction: instruction.draft.trim() }),
            instruction.settle,
          )
        }
      >
        <textarea
          aria-label="Instruction"
          rows={4}
          className={`${TEXT_FIELD} w-full resize-y leading-[1.5]`}
          value={instruction.draft}
          onChange={(e) => instruction.set(e.target.value)}
        />
      </Editable>

      {preset && preset.filterFields.length > 0 && (
        <Editable
          label="When"
          dirty={filter.dirty}
          saving={update.pending && editing === "when"}
          error={sectionError("when")}
          onDiscard={filter.discard}
          onSave={() =>
            void write(
              "when",
              changeOf(sub.id, { filterJson: filterJsonOf(filter.draft, preset.filterFields) }),
              filter.settle,
            )
          }
        >
          <FilterEditor preset={preset} draft={filter.draft} onChange={filter.set} />
        </Editable>
      )}

      <Editable
        label="Where it reports"
        dirty={delivery.dirty}
        saving={update.pending && editing === "delivery"}
        error={sectionError("delivery")}
        onDiscard={delivery.discard}
        onSave={() =>
          void write(
            "delivery",
            changeOf(sub.id, {
              setDelivery: true,
              deliveryChannel: delivery.draft.channel,
              deliveryTarget: delivery.draft.target.trim(),
            }),
            delivery.settle,
          )
        }
      >
        <DeliveryPicker
          source={sub.source}
          channels={channels}
          channel={delivery.draft.channel}
          target={delivery.draft.target}
          onChange={delivery.set}
        />
      </Editable>

      {hasEndpoint(sub) && (
        <EndpointSection
          sub={sub}
          preset={preset}
          steps={detail.senderSteps}
          placeholder={placeholder}
          view={view}
          secret={{
            pending: update.pending && editing === "secret",
            error: sectionError("secret"),
            save: (secret) =>
              write(
                "secret",
                changeOf(sub.id, { setSigningSecret: true, signingSecret: secret }),
                () => {},
              ),
          }}
        />
      )}

      <Block label="Recent events">
        {runs.length === 0 ? (
          <div className="text-[13px] leading-[1.6] text-ink3">
            No events yet. Send a test event to see what a run looks like.
          </div>
        ) : (
          <div className="flex flex-col gap-2">
            {runs.map((run) => (
              <RunRow key={run[0]!.runId || run[0]!.id} run={run} />
            ))}
          </div>
        )}
      </Block>

      <Advanced
        sub={sub}
        runsToday={detail.runsToday}
        saving={update.pending && editing === "limits"}
        error={sectionError("limits")}
        onSave={(change, settle) => void write("limits", change, settle)}
        nameSaving={update.pending && editing === "name"}
        nameError={sectionError("name")}
        onSaveName={(change, settle) => void write("name", change, settle)}
      />
    </>
  );
}

/** One subscription: what it listens for, what Borg does, and what has happened. */
export function SubscriptionDetailPage() {
  const { id = "" } = useParams();
  // Keyed by id: going from one subscription to another must not carry the
  // first one's revealed key, typed drafts or pending test over.
  return <SubscriptionPage key={id} id={id} />;
}

function SubscriptionPage({ id }: { id: string }) {
  const isMobile = useIsMobile();
  const view = useSubscription(id);
  const { query } = view;
  const sub = query.state === "ready" ? query.data.sub : undefined;
  const rename = useMutation(
    (borg, name: string) => borg.admin.updateSubscription(changeOf(id, { name })),
    { onSuccess: () => query.refetch() },
  );

  return (
    <PageChrome
      title={sub?.name ?? "Subscription"}
      renameLabel="Subscription name"
      onRename={sub ? async (name) => (await rename.run(name)) !== undefined : undefined}
    >
      <div className={scrollCol(isMobile, "px-[38px] py-[34px]")}>
        <div className={DETAIL_COL}>
          <BackLink to="/connections" label="Connections" />
          {/* Deleted elsewhere, or an old link: not an outage, so no red box. */}
          {query.state === "error" && query.error.code === GrpcStatus.NotFound ? (
            <div className="text-[13.5px] leading-[1.6] text-ink3">
              This subscription no longer exists.
            </div>
          ) : (
            <DaemonState query={query} label="this subscription">
              {(data) => (
                <Detail
                  detail={data.detail}
                  sub={data.sub}
                  preset={data.preset}
                  channels={data.deliveryChannels}
                  placeholder={data.keyPlaceholder}
                  view={view}
                  renameError={rename.error?.message}
                />
              )}
            </DaemonState>
          )}
        </div>
      </div>
    </PageChrome>
  );
}
