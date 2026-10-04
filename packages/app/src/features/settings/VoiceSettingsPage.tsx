import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import {
  BackLink,
  DETAIL_COL,
  PageChrome,
  Pill,
  SECTION_LABEL,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  isSubmitEnter,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useSettings } from "../../lib/settings";
import { useMutation } from "../../lib/useDaemon";
import { useVoiceCatalog, type VoiceCatalog, type VoiceProvider } from "../../lib/voice";
import { primaryRow, withPrimaryFields, withPrimaryProvider, type ChainRow } from "./voiceChain";

const FIELD =
  "w-full rounded-[10px] border border-field bg-surface2 px-4 py-3 text-[13.5px] leading-none text-ink outline-none placeholder:text-ink3 focus:border-focus";

const CARD = "flex flex-col gap-4 rounded-[12px] border border-line2 bg-surface2 p-4";

/** A labelled control with an optional hint under it. */
function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: ReactNode;
  children: (id: string) => ReactNode;
}) {
  const id = useId();
  return (
    <div className="flex flex-col gap-1.5">
      <label htmlFor={id} className="text-[12.5px] leading-none font-medium text-ink2">
        {label}
      </label>
      {children(id)}
      {hint && <div className="text-[11.5px] leading-[1.45] text-ink3">{hint}</div>}
    </div>
  );
}

/** A text box that saves on Enter or when focus leaves it, and only if changed. */
function TextSetting({
  label,
  hint,
  value,
  placeholder,
  onCommit,
}: {
  label: string;
  hint?: ReactNode;
  value: string;
  placeholder?: string;
  /** Resolves to false when the save did not take, so the same value can be retried. */
  onCommit: (value: string) => Promise<boolean> | void;
}) {
  const [draft, setDraft] = useState(value);
  // The last value handed to `onCommit`, so Enter then blur saves once.
  const sent = useRef(value);
  // A save refreshes the snapshot; follow the stored value.
  useEffect(() => {
    setDraft(value);
    sent.current = value;
  }, [value]);
  const commit = () => {
    const next = draft.trim();
    if (next === sent.current) return;
    sent.current = next;
    void Promise.resolve(onCommit(next)).then((ok) => {
      if (ok === false && sent.current === next) sent.current = value;
    });
  };
  return (
    <Field label={label} hint={hint}>
      {(id) => (
        <input
          id={id}
          className={FIELD}
          value={draft}
          placeholder={placeholder}
          spellCheck={false}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => isSubmitEnter(e) && commit()}
        />
      )}
    </Field>
  );
}

function Switch({
  label,
  desc,
  on,
  onChange,
}: {
  label: string;
  desc: string;
  on: boolean;
  onChange: (on: boolean) => void;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      className="flex w-full cursor-pointer items-center gap-4 border-0 bg-transparent p-0 text-left"
      onClick={() => onChange(!on)}
    >
      <div className="min-w-0 flex-1">
        <div className="text-[13.5px] leading-[1.3] font-medium text-ink">{label}</div>
        <div className="mt-[3px] text-[12px] leading-[1.45] text-ink3">{desc}</div>
      </div>
      {/* Track and knob hold 3:1 against the card and each other in both
          themes (WCAG 1.4.11): the field/focus tokens, and a knob in the
          surface colour, which a white knob on dark-theme neon is not. */}
      <span
        className={`flex h-[22px] w-[38px] flex-none items-center rounded-full p-[3px] transition-colors ${
          on ? "bg-focus" : "bg-field"
        }`}
      >
        <span
          className={`h-4 w-4 rounded-full bg-surface transition-transform ${on ? "translate-x-4" : ""}`}
        />
      </span>
    </button>
  );
}

function ProviderSelect({
  label,
  providers,
  value,
  offLabel,
  onPick,
}: {
  label: string;
  providers: VoiceProvider[];
  value: string;
  /** Adds an "off" choice (empty value) at the top. */
  offLabel?: string;
  onPick: (id: string) => void;
}) {
  const known = value === "" || providers.some((p) => p.id === value);
  return (
    <Field label={label}>
      {(id) => (
        <select id={id} className={FIELD} value={value} onChange={(e) => onPick(e.target.value)}>
          {offLabel !== undefined && <option value="">{offLabel}</option>}
          {offLabel === undefined && value === "" && <option value="">Choose a provider…</option>}
          {!known && <option value={value}>{value}</option>}
          {providers.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
            </option>
          ))}
        </select>
      )}
    </Field>
  );
}

/**
 * The API key for one provider, saved through the same `credentials.{provider}`
 * store the chat providers use — so an OpenAI key saved for chat already
 * serves OpenAI speech, and shows as saved here.
 */
function KeyField({ provider, onSaved }: { provider: VoiceProvider; onSaved: () => void }) {
  const [draft, setDraft] = useState("");
  const saveKey = useMutation(
    (borg, key: string) => borg.admin.setApiKey({ provider: provider.id, literal: key }),
    { onSuccess: onSaved },
  );
  if (!provider.needsKey) return null;
  const submit = () => {
    const key = draft.trim();
    if (!key || saveKey.pending) return;
    void saveKey.run(key).then((r) => r !== undefined && setDraft(""));
  };
  return (
    <Field
      label={provider.keyLabel}
      hint={
        <>
          {provider.hasKey ? "A key is set. Paste a new one to replace it." : "No key yet."}
          {provider.keyUrl && (
            <>
              {" "}
              <a href={provider.keyUrl} target="_blank" rel="noreferrer" className="text-neon-ink">
                Get a key
              </a>
            </>
          )}
          {saveKey.error && <span className="text-alert"> {saveKey.error.message}</span>}
        </>
      }
    >
      {(id) => (
        <div className="flex items-center gap-2">
          <span
            aria-label={provider.hasKey ? "Key set" : "No key"}
            role="img"
            className={`h-[7px] w-[7px] flex-none rounded-full ${provider.hasKey ? "bg-neon" : "bg-alert"}`}
          />
          <input
            id={id}
            type="password"
            autoComplete="off"
            className={FIELD}
            value={draft}
            placeholder={provider.hasKey ? "••••••••" : "Paste key"}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => isSubmitEnter(e) && submit()}
          />
          <Pill variant="primary" onClick={submit}>
            {saveKey.pending ? "Saving…" : "Save"}
          </Pill>
        </div>
      )}
    </Field>
  );
}

/** One chain-backed capability: speech or transcription. */
function ChainSection({
  title,
  toggleKey,
  toggleLabel,
  toggleDesc,
  chainKey,
  providers,
  ready,
  showVoice,
  commandHint,
  refetch,
}: {
  title: string;
  toggleKey: string;
  toggleLabel: string;
  toggleDesc: string;
  chainKey: string;
  providers: VoiceProvider[];
  ready: boolean;
  showVoice: boolean;
  /** How a custom command gets its input and hands back its result. */
  commandHint: string;
  refetch: () => void;
}) {
  const { settings, save } = useSettings();
  const chain = settings.state === "ready" ? settings.data.get(chainKey)?.value : undefined;
  // Each write builds on the last one sent, not on this render's snapshot: a
  // blur save and a provider pick in the same moment would otherwise each
  // drop the other's edit.
  // Writes go out one at a time, in order, and a refresh only resyncs
  // `latest` once none is in flight, so a refresh between two writes
  // cannot roll the second back.
  const latest = useRef(chain);
  const inFlight = useRef(0);
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  useEffect(() => {
    if (inFlight.current === 0) latest.current = chain;
  }, [chain]);
  if (settings.state !== "ready") return null;
  const writeChain = (edit: (json: string | undefined) => string): Promise<boolean> => {
    const next = edit(latest.current);
    latest.current = next;
    inFlight.current += 1;
    const done = queue.current
      .then(() => save(chainKey, next))
      .finally(() => {
        inFlight.current -= 1;
      });
    queue.current = done.catch(() => undefined);
    return done;
  };
  const enabled = settings.data.get(toggleKey)?.value === "true";
  const row: ChainRow | undefined = primaryRow(chain);
  const provider = providers.find((p) => p.id === row?.provider);
  const setField = (field: keyof ChainRow, value: string) =>
    writeChain((json) => withPrimaryFields(json, { [field]: value }));

  return (
    <section>
      <div className={SECTION_LABEL}>{title}</div>
      <div className={CARD}>
        <Switch
          label={toggleLabel}
          desc={toggleDesc}
          on={enabled}
          onChange={(on) => void save(toggleKey, String(on))}
        />
        {enabled && (
          <>
            <ProviderSelect
              label="Provider"
              providers={providers}
              value={row?.provider ?? ""}
              onPick={(id) => void writeChain((json) => withPrimaryProvider(json, id))}
            />
            {provider && <KeyField key={provider.id} provider={provider} onSaved={refetch} />}
            {provider && showVoice && !provider.requiresCommand && (
              <TextSetting
                label="Voice"
                value={row?.voice ?? ""}
                placeholder={provider.defaultVoice || "Provider default"}
                onCommit={(v) => setField("voice", v)}
              />
            )}
            {provider && provider.defaultModel !== "" && (
              <TextSetting
                label="Model"
                value={row?.model ?? ""}
                placeholder={provider.defaultModel}
                onCommit={(v) => setField("model", v)}
              />
            )}
            {provider?.requiresCommand && (
              <TextSetting
                label="Command"
                hint={commandHint}
                value={row?.command ?? ""}
                onCommit={(v) => setField("command", v)}
              />
            )}
            <Status
              ok={ready}
              okText="Ready"
              problem={
                provider
                  ? "Not ready yet: add the missing key or command above."
                  : "Choose a provider to finish."
              }
            />
          </>
        )}
      </div>
    </section>
  );
}

function Status({ ok, okText, problem }: { ok: boolean; okText: string; problem: string }) {
  return (
    <div role="status" className="flex items-center gap-2 text-[12px] leading-[1.45]">
      <span className={`h-[7px] w-[7px] flex-none rounded-full ${ok ? "bg-neon" : "bg-alert"}`} />
      <span className={ok ? "text-ink2" : "text-ink3"}>{ok ? okText : problem}</span>
    </div>
  );
}

function PhoneSection({ catalog, refetch }: { catalog: VoiceCatalog; refetch: () => void }) {
  const { settings, save } = useSettings();
  if (settings.state !== "ready") return null;
  const value = (key: string) => settings.data.get(key)?.value ?? "";
  const chosen = value("telephony.provider");
  const phone = catalog.phone.find((p) => p.provider?.id === chosen);
  const provider = phone?.provider;

  return (
    <section>
      <div className={SECTION_LABEL}>Phone calls</div>
      <div className={CARD}>
        <div className="text-[12.5px] leading-[1.5] text-ink2">
          Let Borg place calls for you: read out a message, or hand the call to a voice agent that
          talks for you and reports back.
        </div>
        <ProviderSelect
          label="Provider"
          providers={catalog.phone.flatMap((p) => (p.provider ? [p.provider] : []))}
          value={chosen}
          offLabel="Off"
          onPick={(id) => void save("telephony.provider", id)}
        />
        {phone && provider && (
          <>
            <KeyField key={provider.id} provider={provider} onSaved={refetch} />
            {phone.usesAccountId && (
              <TextSetting
                label="Account SID"
                value={value("telephony.account_id")}
                placeholder="AC…"
                onCommit={(v) => save("telephony.account_id", v)}
              />
            )}
            <TextSetting
              label={phone.callerIdLabel}
              value={value("telephony.caller_id")}
              onCommit={(v) => save("telephony.caller_id", v)}
            />
            {phone.usesAgentId && (
              <TextSetting
                label={phone.agentRequired ? "Agent id" : "Agent id (optional)"}
                hint={
                  phone.agentRequired
                    ? "The agent that runs the call. Its prompt gets the task as {{task}}."
                    : "Leave empty to give each call's task to a fresh agent. A saved agent gets it as {{task}}."
                }
                value={value("telephony.agent_id")}
                onCommit={(v) => save("telephony.agent_id", v)}
              />
            )}
            <TextSetting
              label="Numbers Borg may call"
              hint="Comma-separated numbers or prefixes with country code, like +1415. Empty allows any number."
              value={value("telephony.allowed_numbers")}
              placeholder="Any number"
              onCommit={(v) => save("telephony.allowed_numbers", v)}
            />
            <Status
              ok={catalog.phoneProblem === ""}
              okText="Ready to call"
              problem={catalog.phoneProblem}
            />
          </>
        )}
      </div>
    </section>
  );
}

/**
 * Settings → Advanced → Voice & phone.
 *
 * Three outcomes, each one switch deep: Borg speaks, Borg listens, Borg
 * calls. Every provider list comes from the daemon's registry, so a provider
 * added there appears here with no change to the app. Power-user knobs (the
 * fallback chain, timeouts, voice-loop tuning) stay in the raw Advanced list.
 */
export function VoiceSettingsPage() {
  const isMobile = useIsMobile();
  const { saveError } = useSettings();
  const catalog = useVoiceCatalog();

  return (
    <PageChrome title="Voice & phone">
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className={DETAIL_COL}>
          <BackLink to="/settings" label="Settings" />
          <div>
            <h2 className="m-0 text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
              Voice & phone
            </h2>
            <div className="mt-2 text-[13.5px] leading-[1.55] text-ink2">
              Talk to Borg, hear it answer, and let it make calls. Pick any provider for each;
              switch whenever you like.
            </div>
          </div>
          {saveError && (
            <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
              {saveError.message}
            </div>
          )}
          <DaemonState query={catalog} label="voice providers">
            {(data) => (
              <>
                <ChainSection
                  title="Voice input"
                  toggleKey="audio.enabled"
                  toggleLabel="Talk to Borg"
                  toggleDesc="Adds a microphone next to the message box, and lets Borg understand voice notes."
                  chainKey="audio.models"
                  providers={data.transcription}
                  ready={data.transcriptionReady}
                  showVoice={false}
                  commandHint="Runs on this machine. Put {audio_file} where the recording's path goes; the command prints the text."
                  refetch={catalog.refetch}
                />
                <ChainSection
                  title="Spoken replies"
                  toggleKey="tts.enabled"
                  toggleLabel="Let Borg speak"
                  toggleDesc="Borg can answer with audio, such as voice replies on Telegram and in voice mode."
                  chainKey="tts.models"
                  providers={data.speech}
                  ready={data.speechReady}
                  showVoice
                  commandHint="Runs on this machine. It reads the reply from {text_file} and writes the audio to {output_path}."
                  refetch={catalog.refetch}
                />
                <PhoneSection catalog={data} refetch={catalog.refetch} />
              </>
            )}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
