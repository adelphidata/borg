import { useState, type ReactNode } from "react";
import { useNavigate } from "react-router-dom";
import { Pill, SECTION_LABEL, TEXT_FIELD } from "@borg/ui";
import type { admin } from "@borg/client";
import { Chevron, ROW_BUTTON } from "../../components/CategoryFold";
import { Switch } from "../../components/Switch";
import { settingPath } from "../settings/catalog";
import {
  BOOL_FIELDS,
  channelLabel,
  LIST_FIELDS,
  ORIGIN_DELIVERY,
  type FilterDraft,
  type Tunnel,
} from "./useSubscriptions";

/** Pieces the add card and the detail page share. */

export const ERROR_LINE = "text-[11.5px] leading-[1.4] text-alert";
export const HELP_LINE = "text-[11.5px] leading-[1.45] text-ink3";
const FIELD_LABEL = "text-[12px] leading-none text-ink2";

/** "issue.created" reads as "issue created"; the dots and underscores are the provider's. */
export function kindLabel(kind: string): string {
  return kind.replace(/[._]/g, " ");
}

/** Severity floors, lowest first, in words. "" is no floor. */
export const SEVERITIES: readonly { id: string; label: string }[] = [
  { id: "", label: "Any" },
  { id: "info", label: "Info and worse" },
  { id: "warning", label: "Warnings and worse" },
  { id: "error", label: "Errors and worse" },
  { id: "critical", label: "Only critical" },
];

/** Label and hint for each filter key the daemon knows. */
const FIELD_COPY: Record<string, { label: string; placeholder?: string }> = {
  kinds: { label: "Kinds of event" },
  severity_min: { label: "How serious" },
  phrase: { label: "Contains", placeholder: "a word or phrase" },
  mention_only: { label: "Only when someone mentions Borg" },
  channel_id: { label: "Channel", placeholder: "channel or chat ID" },
  sender: { label: "From", placeholder: "a user ID" },
  reaction: { label: "Reaction", placeholder: "an emoji name, e.g. eyes" },
  repos: { label: "Repositories", placeholder: "owner/name, owner/name" },
  branch: { label: "Branch", placeholder: "main" },
  labels: { label: "Labels", placeholder: "bug, urgent" },
  author: { label: "Author", placeholder: "a username" },
  exclude_drafts: { label: "Skip draft pull requests" },
};

/** How many conditions show before "More conditions". */
const PRIMARY_FIELDS = 2;

function isSet(value: FilterDraft[string] | undefined): boolean {
  if (value === undefined || value === false) return false;
  if (value === true) return true;
  if (Array.isArray(value)) return value.length > 0;
  return value.trim() !== "";
}

function FilterField({
  field,
  kinds,
  value,
  onChange,
}: {
  field: string;
  kinds: readonly string[];
  value: FilterDraft[string] | undefined;
  onChange: (value: FilterDraft[string]) => void;
}) {
  const copy = FIELD_COPY[field] ?? { label: field };
  const id = `filter-${field}`;

  if (BOOL_FIELDS.has(field)) {
    return (
      <div className="flex items-center gap-3">
        <span className={`${FIELD_LABEL} min-w-0 flex-1`}>{copy.label}</span>
        <Switch on={value === true} onChange={onChange} label={copy.label} />
      </div>
    );
  }

  if (field === "severity_min") {
    return (
      <label htmlFor={id} className="flex flex-col gap-2">
        <span className={FIELD_LABEL}>{copy.label}</span>
        <select
          id={id}
          className={`${TEXT_FIELD} w-full`}
          value={typeof value === "string" ? value : ""}
          onChange={(e) => onChange(e.target.value)}
        >
          {SEVERITIES.map((s) => (
            <option key={s.id} value={s.id}>
              {s.label}
            </option>
          ))}
        </select>
      </label>
    );
  }

  // The source names its kinds, so they are pills to pick from; one it has
  // no list for takes them typed.
  if (field === "kinds" && kinds.length > 0) {
    const picked = Array.isArray(value) ? value : [];
    const all = [...kinds, ...picked.filter((k) => !kinds.includes(k))];
    return (
      <div className="flex flex-col gap-2">
        <span className={FIELD_LABEL}>{copy.label}</span>
        <div role="group" aria-label={copy.label} className="flex flex-wrap gap-1.5">
          {all.map((kind) => {
            const on = picked.includes(kind);
            return (
              <Pill
                key={kind}
                aria-pressed={on}
                variant={on ? "primary" : "default"}
                sizeClasses="px-3 py-1.5 text-[11.5px]"
                onClick={() => onChange(on ? picked.filter((k) => k !== kind) : [...picked, kind])}
              >
                {kindLabel(kind)}
              </Pill>
            );
          })}
        </div>
        <span className={HELP_LINE}>Pick none to hear every kind.</span>
      </div>
    );
  }

  const text = Array.isArray(value) ? value.join(", ") : typeof value === "string" ? value : "";
  return (
    <label htmlFor={id} className="flex flex-col gap-2">
      <span className={FIELD_LABEL}>{copy.label}</span>
      <input
        id={id}
        className={`${TEXT_FIELD} w-full`}
        value={text}
        placeholder={
          copy.placeholder ?? (LIST_FIELDS.has(field) ? "separate with commas" : undefined)
        }
        onChange={(e) => onChange(e.target.value)}
      />
    </label>
  );
}

/**
 * The "When" conditions: only the keys the source supports. The first two
 * show; the rest wait behind "More conditions" unless they already hold a value.
 */
export function FilterEditor({
  preset,
  draft,
  onChange,
}: {
  preset: admin.SubscriptionPreset;
  draft: FilterDraft;
  onChange: (next: FilterDraft) => void;
}) {
  const fields = preset.filterFields;
  const [more, setMore] = useState(false);
  const shown = fields.filter((f, i) => i < PRIMARY_FIELDS || more || isSet(draft[f]));
  const folded = fields.length - shown.length;

  return (
    <div className="flex flex-col gap-4">
      {shown.map((field) => (
        <FilterField
          key={field}
          field={field}
          kinds={preset.kinds}
          value={draft[field]}
          onChange={(value) => onChange({ ...draft, [field]: value })}
        />
      ))}
      {folded > 0 && (
        <button
          type="button"
          aria-expanded={false}
          className={`${ROW_BUTTON} gap-2 p-0 text-[12px] leading-none text-ink2 hover:text-ink`}
          onClick={() => setMore(true)}
        >
          <Chevron open={false} />
          More conditions
        </button>
      )}
      <div className={HELP_LINE}>Keep it narrow. Every match wakes Borg and uses model usage.</div>
    </div>
  );
}

/**
 * The delivery options a source allows, "Only here" first. `channels` are the
 * ones set up on this machine; a stored channel that is not stays listed so
 * the page shows what the subscription really does.
 */
export function deliveryOptions(
  source: string,
  channels: readonly string[],
  current = "",
): { id: string; label: string }[] {
  const named = [...channels];
  if (current !== "" && current !== ORIGIN_DELIVERY && !named.includes(current))
    named.push(current);
  return [
    { id: "", label: "Only here" },
    ...(source === "channel" ? [{ id: ORIGIN_DELIVERY, label: "Reply where it came from" }] : []),
    ...named.map((id) => ({ id, label: channelLabel(id) })),
  ];
}

/** Where a run's report goes: its own conversation, the origin chat, or a channel. */
export function DeliveryPicker({
  source,
  channels,
  channel,
  target,
  onChange,
}: {
  source: string;
  /** Delivery channels set up on this machine. */
  channels: readonly string[];
  channel: string;
  target: string;
  onChange: (next: { channel: string; target: string }) => void;
}) {
  const options = deliveryOptions(source, channels, channel);
  const named =
    channel !== "" && channel !== ORIGIN_DELIVERY
      ? { id: channel, label: channelLabel(channel) }
      : undefined;
  return (
    <div className="flex flex-col gap-3">
      <div role="group" aria-label="Where it reports" className="flex flex-wrap gap-1.5">
        {options.map((o) => (
          <Pill
            key={o.id || "here"}
            aria-pressed={channel === o.id}
            variant={channel === o.id ? "primary" : "default"}
            sizeClasses="px-3 py-1.5 text-[11.5px]"
            onClick={() => onChange({ channel: o.id, target: o.id === channel ? target : "" })}
          >
            {o.label}
          </Pill>
        ))}
      </div>
      {named && (
        <input
          aria-label={`Who gets it on ${named.label}`}
          className={`${TEXT_FIELD} w-full`}
          placeholder="channel or chat ID"
          value={target}
          onChange={(e) => onChange({ channel, target: e.target.value })}
        />
      )}
      <div className={HELP_LINE}>
        {channel === ""
          ? "Every run stays in this subscription's own conversation."
          : channel === ORIGIN_DELIVERY
            ? "Borg answers in the chat the message came from."
            : `Borg also sends each report to ${named?.label ?? channel}.`}
      </div>
    </div>
  );
}

/** Paste box for the secret a provider shows after the endpoint is added there. */
export function SigningSecretField({
  provider,
  saved,
  error,
  pending,
  onSave,
}: {
  provider: string;
  /** A secret is already stored. */
  saved: boolean;
  error: string | undefined;
  pending: boolean;
  /** Resolves true once stored. */
  onSave: (secret: string) => Promise<boolean>;
}) {
  const [secret, setSecret] = useState("");
  return (
    <div className="flex flex-col gap-2">
      <div className={SECTION_LABEL}>Signing secret</div>
      <div className={HELP_LINE}>
        {saved
          ? `Saved. Borg checks every event against it. Paste a new one if ${provider} replaces it.`
          : `${provider} shows a signing secret after you add the address there. Paste it here so Borg can tell its events are real.`}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <input
          type="password"
          aria-label="Signing secret"
          autoComplete="off"
          className={`${TEXT_FIELD} min-w-0 flex-1`}
          value={secret}
          onChange={(e) => setSecret(e.target.value)}
        />
        <Pill
          square
          disabled={pending || secret.trim() === ""}
          onClick={() => {
            void onSave(secret.trim()).then((ok) => ok && setSecret(""));
          }}
        >
          {pending ? "Saving…" : "Save secret"}
        </Pill>
      </div>
      {error && (
        <div role="alert" className={ERROR_LINE}>
          {error}
        </div>
      )}
    </div>
  );
}

/**
 * Says when the address only works on this computer, and offers the tunnel
 * that fixes it. Nothing at all when the address is public.
 */
export function Reachability({
  endpoint,
  tunnel,
}: {
  endpoint: admin.SubscriptionEndpoint | undefined;
  tunnel: Tunnel;
}) {
  const navigate = useNavigate();
  if (!endpoint || endpoint.url === "" || endpoint.public) return null;
  return (
    <div className="flex flex-col gap-2.5 rounded-[10px] border border-warn bg-warn-soft px-4 py-3">
      <div className="flex flex-wrap items-center gap-3">
        <span className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
          {endpoint.hint || "Only this computer can reach this address."}
        </span>
        <Pill
          square
          variant="primary"
          disabled={tunnel.pending}
          onClick={() => void tunnel.turnOn()}
        >
          {tunnel.pending ? "Turning on…" : "Turn on"}
        </Pill>
      </div>
      {tunnel.unconfigured && (
        <div className="flex flex-wrap items-center gap-3">
          <span className={`${HELP_LINE} min-w-0 flex-1`}>
            Borg has no tunnel chosen yet. Pick one in Settings, then turn it on here.
          </span>
          <Pill square onClick={() => navigate(settingPath("tunnel.provider"))}>
            Choose a tunnel
          </Pill>
        </div>
      )}
      {tunnel.error && (
        <div role="alert" className={ERROR_LINE}>
          {tunnel.error}
        </div>
      )}
    </div>
  );
}

/** A small titled block inside a card or page. */
export function Block({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section>
      <div className={SECTION_LABEL}>{label}</div>
      {children}
    </section>
  );
}
