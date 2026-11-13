import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Pill, TEXT_FIELD, isSubmitEnter } from "@borg/ui";
import { useSettings } from "../../lib/settings";
import { useMutation } from "../../lib/useDaemon";
import type { VoiceProvider } from "../../lib/voice";
import { primaryRow, withPrimaryFields, type ChainRow } from "./voiceChain";

/**
 * The controls the guided settings pages (Voice & phone, Meetings) are built
 * from. Each saves through `useSettings` the moment it is changed; none of
 * them knows which setting it edits.
 */

export const FIELD = `w-full ${TEXT_FIELD}`;

export const CARD = "flex flex-col gap-4 rounded-[12px] border border-line2 bg-surface2 p-4";

/** A labelled control with an optional hint under it. */
export function Field({
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
export function TextSetting({
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
  // A save refreshes the snapshot; follow the stored value. Only on a real
  // change: the mount run lands after first paint, and would wipe whatever
  // was typed before it.
  const shown = useRef(value);
  useEffect(() => {
    if (shown.current === value) return;
    shown.current = value;
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

export function Switch({
  label,
  warning,
  on,
  onChange,
}: {
  label: string;
  /** Shown only when the switch cannot do its job as set. */
  warning?: string;
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
        {warning && <div className="mt-[3px] text-[12px] leading-[1.45] text-alert">{warning}</div>}
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

export function ProviderSelect({
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
export function KeyField({ provider, onSaved }: { provider: VoiceProvider; onSaved: () => void }) {
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
          {!provider.hasKey && "No key yet."}
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

export function SelectSetting({
  label,
  hint,
  value,
  options,
  onPick,
}: {
  label: string;
  hint?: ReactNode;
  value: string;
  options: [string, string][];
  onPick: (value: string) => void;
}) {
  const known = options.some(([v]) => v === value);
  return (
    <Field label={label} hint={hint}>
      {(id) => (
        <select id={id} className={FIELD} value={value} onChange={(e) => onPick(e.target.value)}>
          {!known && <option value={value}>{value}</option>}
          {options.map(([v, text]) => (
            <option key={v} value={v}>
              {text}
            </option>
          ))}
        </select>
      )}
    </Field>
  );
}

/** The fold for settings a first-time user never needs. */
export function MoreOptions({ children }: { children: ReactNode }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="flex flex-col gap-4 border-t border-line2 pt-4">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent p-0 text-left text-[12.5px] leading-none font-medium text-ink2"
      >
        <span
          className={`inline-block transition-transform ${open ? "rotate-90" : ""}`}
          aria-hidden="true"
        >
          ›
        </span>
        More options
      </button>
      {open && children}
    </div>
  );
}

/** What `useChain` hands back. */
export interface Chain {
  /** The row that answers first, if there is one. */
  row: ChainRow | undefined;
  writeChain: (edit: (json: string | undefined) => string) => Promise<boolean>;
  setField: (field: keyof ChainRow, value: string) => Promise<boolean>;
}

/**
 * Edits the first row of a provider chain, one write at a time, each on
 * top of the last: a blur save and a provider pick in the same moment would
 * otherwise each drop the other's edit, and a refresh between two writes
 * cannot roll the second back.
 */
export function useChain(chainKey: string): Chain {
  const { settings, save } = useSettings();
  const chain = settings.state === "ready" ? settings.data.get(chainKey)?.value : undefined;
  const latest = useRef(chain);
  const inFlight = useRef(0);
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  useEffect(() => {
    if (inFlight.current === 0) latest.current = chain;
  }, [chain]);
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
  const row: ChainRow | undefined = primaryRow(chain);
  const setField = (field: keyof ChainRow, value: string) =>
    writeChain((json) => withPrimaryFields(json, { [field]: value }));
  return { row, writeChain, setField };
}

export function Status({ ok, okText, problem }: { ok: boolean; okText: string; problem: string }) {
  return (
    <div role="status" className="flex items-center gap-2 text-[12px] leading-[1.45]">
      <span className={`h-[7px] w-[7px] flex-none rounded-full ${ok ? "bg-neon" : "bg-alert"}`} />
      <span className={ok ? "text-ink2" : "text-ink3"}>{ok ? okText : problem}</span>
    </div>
  );
}
