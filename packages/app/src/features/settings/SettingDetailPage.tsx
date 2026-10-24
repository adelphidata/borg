import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { Link, useLocation, useParams } from "react-router-dom";
import {
  BackLink,
  CheckIcon,
  DETAIL_COL,
  PageChrome,
  Pill,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  TEXT_FIELD,
  isSubmitEnter,
  scrollCol,
  useIsMobile,
  useTheme,
} from "@borg/ui";
import type { ThemePreference } from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { ShuffleButton } from "../../components/NameField";
import { DEFAULT_AGENT_NAME, rerollName } from "../../lib/agentName";
import { useSettings, type DaemonSetting } from "../../lib/settings";
import { MAX_NAME_CHARS, USER_NAME_KEY } from "../../lib/userName";
import {
  effectiveModel,
  modelsOf,
  providerOf,
  useSetupStatus,
  type SetupStatus,
} from "../onboarding/useSetupStatus";
import { useAutoGrow } from "../../lib/useAutoGrow";
import { ProviderSetup } from "./ProviderSetup";
import { findBasicSetting, optionLabel, type BasicSetting, type SettingOption } from "./catalog";
import { compactJson, groupDigits, prettyJson, ungroupDigits, valueShape } from "./settingValue";

/** The row that opens the free-text editor under a catalog picker. */
const SOMETHING_ELSE = "__something_else__";

/** Where the back link leads: Settings, unless the page that opened this one said otherwise. */
function useBackLink(): { to: string; label: string } {
  const state = useLocation().state as { back?: { to: string; label: string } } | null;
  return state?.back ?? { to: "/settings", label: "Settings" };
}

function DetailShell({ title, children }: { title: string; children: ReactNode }) {
  const isMobile = useIsMobile();
  const back = useBackLink();
  return (
    <PageChrome title={title}>
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className={DETAIL_COL}>
          <BackLink to={back.to} label={back.label} />
          {children}
        </div>
      </div>
    </PageChrome>
  );
}

function Heading({ name }: { name: string }) {
  return (
    <h2 className="m-0 text-[24px] leading-[1.2] font-light tracking-[-0.025em] break-all text-ink">
      {name}
    </h2>
  );
}

function SaveError({ message }: { message: string | undefined }) {
  if (!message) return null;
  return (
    <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
      {message}
    </div>
  );
}

/** An on/off key takes only these two; a free field would accept anything. */
const BOOL_OPTIONS: SettingOption[] = [
  { value: "true", label: "On" },
  { value: "false", label: "Off" },
];

function OptionPicker({
  options,
  value,
  disabled,
  onPick,
}: {
  options: SettingOption[];
  value: string;
  disabled: boolean;
  onPick: (value: string) => void;
}) {
  return (
    <div
      className={`overflow-hidden rounded-[10px] border border-line2 bg-surface2 ${
        disabled ? "pointer-events-none opacity-60" : ""
      }`}
    >
      {options.map((o) => {
        const on = o.value === value;
        return (
          <button
            type="button"
            key={o.value}
            aria-pressed={on}
            disabled={disabled}
            className="flex w-full cursor-pointer items-center gap-4 border-0 border-b border-line2 bg-transparent px-4 py-3.5 text-left transition-[background] duration-150 last:border-b-0 hover:bg-surface3"
            onClick={() => onPick(o.value)}
          >
            <div className="min-w-0 flex-1">
              <div
                className={`text-[13.5px] leading-[1.3] font-medium ${
                  on ? "text-neon-ink" : "text-ink"
                }`}
              >
                {optionLabel(o)}
              </div>
              {o.desc && (
                <div className="mt-[3px] text-[12px] leading-[1.45] text-ink3">{o.desc}</div>
              )}
            </div>
            {on && <CheckIcon size={15} role="img" aria-label="Selected" />}
          </button>
        );
      })}
    </div>
  );
}

function ValueEditor({
  label,
  initial,
  saving,
  onSave,
  onReset,
}: {
  /** The field's accessible name: the page heading names it only by sight. */
  label: string;
  initial: string;
  saving: boolean;
  onSave: (value: string) => void;
  /** Present only when the key is overridden and can go back to its default. */
  onReset?: () => void;
}) {
  const [draft, setDraft] = useState(initial);
  // A save refreshes the snapshot; follow the new effective value. Synced
  // during render, not in an effect: an effect runs after paint, and a
  // keystroke landing in that window would be overwritten by the seed.
  const [seeded, setSeeded] = useState(initial);
  if (initial !== seeded) {
    setSeeded(initial);
    setDraft(initial);
  }

  return (
    <div className="flex flex-col gap-3.5">
      <input
        className={`w-full ${TEXT_FIELD}`}
        value={draft}
        aria-label={label}
        spellCheck={false}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => isSubmitEnter(e) && !saving && onSave(draft)}
      />
      <div className="flex items-center gap-[9px]">
        <Pill variant="primary" onClick={() => !saving && onSave(draft)}>
          {saving ? "Saving…" : "Save"}
        </Pill>
        {onReset && <Pill onClick={() => !saving && onReset()}>Reset to default</Pill>}
      </div>
    </div>
  );
}

/** Tall enough for a dozen-entry list; past it the box scrolls. */
const JSON_MAX_PX = 420;

/**
 * A JSON list or object, laid out one entry per line.
 *
 * On one line, `security.blocked_paths` ran off the edge of the field after
 * the fourth path, and nobody can edit what they cannot see. Monospace keeps
 * the quotes upright and the indentation honest. A draft that does not parse,
 * or no longer has the list or object shape the setting stores, is stopped
 * here, in plain words, instead of reaching the daemon as a string it rejects
 * with a parser trace.
 */
function JsonEditor({
  label,
  initial,
  container,
  saving,
  onSave,
  onReset,
}: {
  label: string;
  /** Already pretty-printed. */
  initial: string;
  /** What the stored value is, and so what a save must stay. */
  container: "list" | "object";
  saving: boolean;
  /** Receives the compact form the daemon stores. */
  onSave: (value: string) => void;
  onReset?: () => void;
}) {
  const [draft, setDraft] = useState(initial);
  const [invalid, setInvalid] = useState<string>();
  // Synced during render for the same reason as ValueEditor.
  const [seeded, setSeeded] = useState(initial);
  if (initial !== seeded) {
    setSeeded(initial);
    setDraft(initial);
    setInvalid(undefined);
  }
  const ref = useAutoGrow(draft, JSON_MAX_PX);

  const commit = () => {
    if (saving) return;
    const out = compactJson(draft, container);
    if ("error" in out) {
      setInvalid(out.error);
      return;
    }
    onSave(out.value);
  };

  return (
    <div className="flex flex-col gap-3.5">
      <textarea
        ref={ref}
        aria-label={label}
        aria-invalid={invalid ? true : undefined}
        spellCheck={false}
        value={draft}
        className={`w-full resize-none overflow-y-auto rounded-[10px] border bg-surface2 px-4 py-3 font-mono text-[12.5px] leading-[1.6] text-ink outline-none [tab-size:2] focus:border-focus ${
          invalid ? "border-alert" : "border-field"
        }`}
        onChange={(e) => {
          setDraft(e.target.value);
          setInvalid(undefined);
        }}
        // Enter is a new line here. The modifier saves, as in any code box.
        onKeyDown={(e) => {
          if ((e.metaKey || e.ctrlKey) && isSubmitEnter(e)) {
            e.preventDefault();
            commit();
          }
        }}
      />
      {invalid && (
        <div role="alert" className="text-[12.5px] leading-[1.45] text-alert">
          {invalid}
        </div>
      )}
      <div className="flex items-center gap-[9px]">
        <Pill variant="primary" onClick={commit}>
          {saving ? "Saving…" : "Save"}
        </Pill>
        {onReset && <Pill onClick={() => !saving && onReset()}>Reset to default</Pill>}
      </div>
    </div>
  );
}

/**
 * The editor for a raw registry key, picked by what the value looks like.
 *
 * A count reads with its thousands grouped and saves without them; a list or
 * object gets the multi-line JSON box; everything else is the plain field.
 */
function ShapedValueEditor({
  label,
  settingKey,
  kind,
  initial,
  saving,
  onSave,
  onReset,
}: {
  label: string;
  settingKey: string;
  /** The daemon's type for a curated key; empty for the rest. */
  kind: string;
  initial: string;
  saving: boolean;
  onSave: (value: string) => void;
  onReset?: () => void;
}) {
  const shape = valueShape(settingKey, initial, kind);
  if (shape.kind === "json") {
    return (
      <JsonEditor
        label={label}
        initial={prettyJson(shape.parsed)}
        container={Array.isArray(shape.parsed) ? "list" : "object"}
        saving={saving}
        onSave={onSave}
        onReset={onReset}
      />
    );
  }
  const integer = shape.kind === "integer";
  return (
    <ValueEditor
      label={label}
      initial={integer ? groupDigits(initial) : initial}
      saving={saving}
      onSave={(v) => onSave(integer ? ungroupDigits(v) : v)}
      onReset={onReset}
    />
  );
}

/**
 * Options for a provider or model row, from the daemon's catalog.
 *
 * Empty when the catalog has not answered, which is the signal to fall back
 * to the free-text editor rather than an empty picker.
 */
function catalogOptions(
  kind: BasicSetting["catalog"],
  status: SetupStatus,
  provider: string,
): SettingOption[] {
  if (kind === "provider") {
    return status.providers.map((p) => ({ value: p.id, label: p.label }));
  }
  return modelsOf(status.providers, provider).map((m) => ({
    value: m.id,
    label: m.label,
    desc: m.note || undefined,
  }));
}

/**
 * Provider and model, picked from the same catalog the first run showed.
 *
 * The curated rows are the whole picker for almost everyone; "Something
 * else…" opens the plain field for an id the catalog does not carry, so the
 * power user loses nothing and the first-time user never meets a text box.
 * Under the provider picker sits what the provider in force needs — its
 * settings, its runtime, its keys — so a provider with nothing behind it
 * says so here, with the way to fix it, instead of failing on the next
 * message.
 */
function CatalogDetail({
  basic,
  entry,
  provider,
  model,
  settings,
}: {
  basic: BasicSetting;
  entry: { key: string; value: string; overridden: boolean };
  /** The provider in force, which decides the model list. */
  provider: string;
  /** The model in force, which a provider switch may have to replace. */
  model: string;
  /** The whole registry, for the provider's own settings. */
  settings: Map<string, DaemonSetting>;
}) {
  const { save, clear, saving } = useSettings();
  const setup = useSetupStatus();
  // Undecided until the user says; a value the catalog does not know starts
  // as text, and "Back to the list" is still honoured after that.
  const [freeText, setFreeText] = useState<boolean>();
  // The provider a switch moved to when its model could not follow: the
  // write failed, or the provider has no list to take a default from.
  const [modelStranded, setModelStranded] = useState<{ label: string; typed: boolean }>();
  // The new provider's setup sits under a long list, below the fold; a
  // switch brings it into view so a missing key is seen before a message.
  const setupRef = useRef<HTMLDivElement>(null);

  const status = setup.state === "ready" ? setup.data : undefined;
  const options = status ? catalogOptions(basic.catalog, status, provider) : [];
  // An empty model runs as the provider default, so that row is the one on.
  const current =
    status && basic.catalog === "model"
      ? effectiveModel(status.providers, provider, entry.value)
      : entry.value;
  const listed = options.some((o) => o.value === current);
  const editing = options.length === 0 || (freeText ?? (!listed && entry.value !== ""));

  // A model belongs to its provider. Keeping `claude-haiku-4-5` across a
  // switch to OpenAI sends OpenAI a model it has never heard of, so a switch
  // to a provider that does not list the current model takes its default too.
  const pick = async (value: string) => {
    setModelStranded(undefined);
    if (!(await save(entry.key, value)) || basic.catalog !== "provider" || !status) return;
    setupRef.current?.scrollIntoView?.({ behavior: "smooth", block: "start" });
    const next = providerOf(status.providers, value);
    const known = modelsOf(status.providers, value).some((m) => m.id === model);
    if (!next || known) return;
    if (next.defaultModel === "") {
      // No list to take a default from (a custom endpoint, a Vertex
      // project): the model is typed, and the old one belongs elsewhere.
      setModelStranded({ label: next.label, typed: true });
    } else if (!(await save("model", next.defaultModel))) {
      // Two writes, and only the first landed: the new provider now has a
      // model it does not carry. Say so, and where to fix it.
      setModelStranded({ label: next.label, typed: false });
    }
  };

  return (
    <>
      {!editing && (
        <OptionPicker
          options={[...options, { value: SOMETHING_ELSE, label: "Something else…" }]}
          value={current}
          disabled={saving}
          onPick={(v) => {
            if (v === SOMETHING_ELSE) setFreeText(true);
            else if (v !== current) void pick(v);
          }}
        />
      )}
      {editing && (
        <ValueEditor
          label={basic.name}
          initial={entry.value}
          saving={saving}
          onSave={(v) => void pick(v)}
          onReset={entry.overridden ? () => void clear(entry.key) : undefined}
        />
      )}
      {editing && options.length > 0 && (
        <button
          type="button"
          className="cursor-pointer self-start border-0 bg-transparent p-0 text-[12.5px] leading-none text-ink3 hover:text-ink"
          onClick={() => setFreeText(false)}
        >
          Back to the list
        </button>
      )}
      {modelStranded && (
        <div className="rounded-[10px] border border-line2 bg-surface2 px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
          {modelStranded.typed
            ? `Now using ${modelStranded.label}. Name the model it should run.`
            : `Now using ${modelStranded.label}, but its model could not be switched.`}{" "}
          <Link to="/settings/model" className="text-neon-ink no-underline">
            {modelStranded.typed ? "Set the model" : "Pick a model"}
          </Link>
        </div>
      )}
      {basic.catalog === "provider" && (
        <div ref={setupRef} className="flex scroll-mt-6 flex-col gap-7">
          <ProviderSetup provider={entry.value} settings={settings} />
        </div>
      )}
    </>
  );
}

type Entry = { key: string; value: string; overridden: boolean };

/**
 * A draft that follows its stored value: a save refreshes the snapshot, and
 * the field takes the new value. Synced during render, not in an effect, so a
 * keystroke before the first effect could run is kept.
 */
function useSeededDraft(stored: string) {
  const [draft, setDraft] = useState(stored);
  const [seeded, setSeeded] = useState(stored);
  if (stored !== seeded) {
    setSeeded(stored);
    setDraft(stored);
  }
  return [draft, setDraft] as const;
}

const NAME_LABEL = "text-[12.5px] leading-none font-medium text-ink2";
const NAME_BOX =
  "flex items-center gap-1 rounded-[10px] border border-field bg-surface2 py-1 pr-1.5 pl-4 focus-within:border-focus";
const NAME_INPUT =
  "min-w-0 flex-1 bg-transparent py-2 text-[13.5px] leading-none text-ink outline-none placeholder:text-ink3";

/**
 * Both names: what the agent answers to, typed or rerolled from the pool the
 * first run drew from, and what it calls the user. A reroll only fills the
 * field; Save is the commitment, and writes only the names that changed. A
 * field cleared and saved goes back to its default: "Borg" for the agent, no
 * name for the user. The daemon carries either into the memory files, and the
 * agent fills in the user's name itself when they mention it.
 */
function NameDetail({ agent, user }: { agent: Entry; user: Entry | undefined }) {
  const { save, clear, saving } = useSettings();
  const setup = useSetupStatus();
  const pool = setup.state === "ready" ? setup.data.agentNames : [];
  const agentId = useId();
  const userId = useId();

  const agentName = agent.value.trim() || DEFAULT_AGENT_NAME;
  const userName = user?.value.trim() ?? "";
  const [agentDraft, setAgentDraft] = useSeededDraft(agentName);
  const [userDraft, setUserDraft] = useSeededDraft(userName);

  const nextAgent = agentDraft.trim();
  const nextUser = userDraft.trim();
  const agentDirty = nextAgent === "" ? agent.overridden : nextAgent !== agentName;
  const userDirty = user !== undefined && nextUser !== userName;
  const dirty = agentDirty || userDirty;

  const write = (key: string, value: string) => (value === "" ? clear(key) : save(key, value));
  const commit = async () => {
    if (!dirty || saving) return;
    // One at a time, and stop at the first refusal: its error is the one on screen.
    if (agentDirty && !(await write(agent.key, nextAgent))) return;
    if (userDirty) await write(USER_NAME_KEY, nextUser);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => isSubmitEnter(e) && void commit();

  return (
    <>
      <div className="flex flex-col gap-5">
        <div className="flex flex-col gap-1.5">
          <label htmlFor={agentId} className={NAME_LABEL}>
            Agent name
          </label>
          <div className={NAME_BOX}>
            <input
              id={agentId}
              className={NAME_INPUT}
              value={agentDraft}
              placeholder={DEFAULT_AGENT_NAME}
              maxLength={MAX_NAME_CHARS}
              spellCheck={false}
              onChange={(e) => setAgentDraft(e.target.value)}
              onKeyDown={onKeyDown}
            />
            <ShuffleButton
              disabled={pool.length === 0}
              onClick={() => setAgentDraft(rerollName(pool, agentDraft))}
            />
          </div>
        </div>
        {user && (
          <div className="flex flex-col gap-1.5">
            <label htmlFor={userId} className={NAME_LABEL}>
              Your name
            </label>
            <div className={NAME_BOX}>
              <input
                id={userId}
                className={NAME_INPUT}
                value={userDraft}
                placeholder="What should Borg call you?"
                maxLength={MAX_NAME_CHARS}
                autoComplete="name"
                spellCheck={false}
                onChange={(e) => setUserDraft(e.target.value)}
                onKeyDown={onKeyDown}
              />
            </div>
          </div>
        )}
        <div className="flex items-center gap-[9px]">
          <Pill variant="primary" disabled={!dirty || saving} onClick={() => void commit()}>
            {saving ? "Saving…" : "Save"}
          </Pill>
        </div>
      </div>
    </>
  );
}

/** The local-only theme setting — applies before paint, so it never touches borgd. */
function AppearanceDetail() {
  const { preference, setPreference } = useTheme();
  const setting = findBasicSetting("appearance")!;

  return (
    <DetailShell title={setting.name}>
      <Heading name={setting.name} />
      <OptionPicker
        options={setting.options ?? []}
        value={preference}
        disabled={false}
        onPick={(picked) => {
          // The options are built from `THEME_PREFERENCES`, so `picked` is one.
          if (picked !== preference) setPreference(picked as ThemePreference);
        }}
      />
    </DetailShell>
  );
}

/** One curated setting or one raw registry key, expanded into an editor. */
export function SettingDetailPage() {
  const { id = "", key: rawKey = "" } = useParams();
  const { settings, save, clear, saving, saveError, resetSaveError } = useSettings();

  const basic = rawKey ? undefined : findBasicSetting(id);
  const key = rawKey ? decodeURIComponent(rawKey) : (basic?.key ?? null);

  // A failure belongs to the write that caused it: arriving must not show one
  // left by another surface (an error card's inline fix), and leaving should
  // not carry this page's to the next one.
  useEffect(() => {
    resetSaveError();
    return resetSaveError;
  }, [resetSaveError]);

  if (basic && basic.key === null) return <AppearanceDetail />;

  if (!basic && !rawKey) {
    return (
      <DetailShell title="Settings">
        <div className="text-[14.5px] leading-[1.62] text-pretty text-ink2">
          That setting doesn’t exist.
        </div>
      </DetailShell>
    );
  }

  const title = basic?.name ?? key ?? "Settings";

  return (
    <DetailShell title={title}>
      <DaemonState query={settings} label="settings">
        {(map) => {
          const entry = key ? map.get(key) : undefined;
          if (!entry) {
            return (
              <div className="text-[14.5px] leading-[1.62] text-pretty text-ink2">
                That setting doesn’t exist.
              </div>
            );
          }

          if (basic?.catalog === "name") {
            return (
              <>
                <Heading name={title} />
                <SaveError message={saveError?.message} />
                <NameDetail agent={entry} user={map.get(USER_NAME_KEY)} />
              </>
            );
          }

          if (basic?.catalog) {
            return (
              <>
                <Heading name={title} />
                <SaveError message={saveError?.message} />
                <CatalogDetail
                  basic={basic}
                  entry={entry}
                  provider={map.get("provider")?.value ?? ""}
                  model={map.get("model")?.value ?? ""}
                  settings={map}
                />
              </>
            );
          }

          return (
            <>
              <Heading name={title} />
              <SaveError message={saveError?.message} />
              {basic?.options || entry.kind === "bool" ? (
                <div className="flex flex-col items-start gap-3">
                  <div className="self-stretch">
                    <OptionPicker
                      options={basic?.options ?? BOOL_OPTIONS}
                      value={entry.value}
                      disabled={saving}
                      onPick={(v) => v !== entry.value && void save(entry.key, v)}
                    />
                  </div>
                  {entry.overridden && (
                    <Pill onClick={() => !saving && void clear(entry.key)}>Reset to default</Pill>
                  )}
                </div>
              ) : (
                <ShapedValueEditor
                  label={title}
                  settingKey={entry.key}
                  kind={entry.kind}
                  initial={entry.value}
                  saving={saving}
                  onSave={(v) => void save(entry.key, v)}
                  onReset={entry.overridden ? () => void clear(entry.key) : undefined}
                />
              )}
            </>
          );
        }}
      </DaemonState>
    </DetailShell>
  );
}
