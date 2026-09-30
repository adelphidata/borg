import { useEffect, useState, type ReactNode } from "react";
import { Link, useParams } from "react-router-dom";
import {
  BackLink,
  CheckIcon,
  DETAIL_COL,
  PageChrome,
  Pill,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  isSubmitEnter,
  scrollCol,
  useIsMobile,
  useTheme,
} from "@borg/ui";
import type { ThemePreference } from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useSettings } from "../../lib/settings";
import {
  modelsOf,
  providerLabel,
  providerOf,
  useSetupStatus,
  type SetupStatus,
} from "../onboarding/useSetupStatus";
import { findBasicSetting, optionLabel, type BasicSetting, type SettingOption } from "./catalog";

/** The row that opens the free-text editor under a catalog picker. */
const SOMETHING_ELSE = "__something_else__";

function DetailShell({ title, children }: { title: string; children: ReactNode }) {
  const isMobile = useIsMobile();
  return (
    <PageChrome title={title}>
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className={DETAIL_COL}>
          <BackLink to="/settings" label="Settings" />
          {children}
        </div>
      </div>
    </PageChrome>
  );
}

function Heading({ name, desc }: { name: string; desc?: string }) {
  return (
    <div>
      <div className="text-[24px] leading-[1.2] font-light tracking-[-0.025em] break-all text-ink">
        {name}
      </div>
      {desc && <div className="mt-2 text-[13.5px] leading-[1.55] text-ink2">{desc}</div>}
    </div>
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
  initial,
  saving,
  onSave,
  onReset,
}: {
  initial: string;
  saving: boolean;
  onSave: (value: string) => void;
  /** Present only when the key is overridden and can go back to its default. */
  onReset?: () => void;
}) {
  const [draft, setDraft] = useState(initial);
  // A save refreshes the snapshot; follow the new effective value.
  useEffect(() => setDraft(initial), [initial]);

  return (
    <div className="flex flex-col gap-3.5">
      <input
        className="w-full rounded-[10px] border border-line2 bg-surface2 px-4 py-3 text-[13.5px] leading-none text-ink outline-none placeholder:text-ink3 focus:border-neon"
        value={draft}
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
    return status.providers.map((p) => ({ value: p.id, label: p.label, desc: p.blurb }));
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
 * A provider with no key behind it says so here, with the way to fix it,
 * instead of failing on the next message.
 */
function CatalogDetail({
  basic,
  entry,
  provider,
  model,
}: {
  basic: BasicSetting;
  entry: { key: string; value: string; overridden: boolean };
  /** The provider in force, which decides the model list. */
  provider: string;
  /** The model in force, which a provider switch may have to replace. */
  model: string;
}) {
  const { save, clear, saving } = useSettings();
  const setup = useSetupStatus();
  // Undecided until the user says; a value the catalog does not know starts
  // as text, and "Back to the list" is still honoured after that.
  const [freeText, setFreeText] = useState<boolean>();

  const status = setup.state === "ready" ? setup.data : undefined;
  const options = status ? catalogOptions(basic.catalog, status, provider) : [];
  const listed = options.some((o) => o.value === entry.value);
  const editing = options.length === 0 || (freeText ?? (!listed && entry.value !== ""));

  const chosen =
    status && basic.catalog === "provider" ? providerOf(status.providers, entry.value) : undefined;
  const keyMissing =
    chosen !== undefined &&
    chosen.needsApiKey &&
    !status!.detected.some((d) => d.provider.toLowerCase() === chosen.id.toLowerCase());

  // A model belongs to its provider. Keeping `claude-haiku-4-5` across a
  // switch to OpenAI sends OpenAI a model it has never heard of, so a switch
  // to a provider that does not list the current model takes its default too.
  const pick = async (value: string) => {
    if (!(await save(entry.key, value)) || basic.catalog !== "provider" || !status) return;
    const next = providerOf(status.providers, value);
    const known = modelsOf(status.providers, value).some((m) => m.id === model);
    if (next && next.defaultModel !== "" && !known) await save("model", next.defaultModel);
  };

  return (
    <>
      {!editing && (
        <OptionPicker
          options={[...options, { value: SOMETHING_ELSE, label: "Something else…" }]}
          value={entry.value}
          disabled={saving}
          onPick={(v) => {
            if (v === SOMETHING_ELSE) setFreeText(true);
            else if (v !== entry.value) void pick(v);
          }}
        />
      )}
      {editing && (
        <ValueEditor
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
      {keyMissing && (
        <div className="rounded-[10px] border border-line2 bg-surface2 px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
          No key saved for {providerLabel(status!.providers, chosen.id)} yet.{" "}
          <Link to={`/connections/provider/${chosen.id}`} className="text-neon-ink no-underline">
            Add one
          </Link>
        </div>
      )}
      <div className="pt-3.5 text-[13px] leading-[1.6] text-pretty text-ink3">
        {editing
          ? "Saved to the daemon. Invalid values are rejected with the reason above."
          : "Changes apply straight away. Nothing to save."}
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
      <Heading name={setting.name} desc={setting.desc} />
      <OptionPicker
        options={setting.options ?? []}
        value={preference}
        disabled={false}
        onPick={(picked) => {
          // The options are built from `THEME_PREFERENCES`, so `picked` is one.
          if (picked !== preference) setPreference(picked as ThemePreference);
        }}
      />
      <div className="pt-3.5 text-[13px] leading-[1.6] text-pretty text-ink3">
        Changes apply straight away. Nothing to save.
      </div>
    </DetailShell>
  );
}

/** One curated setting or one raw registry key, expanded into an editor. */
export function SettingDetailPage() {
  const { id = "", key: rawKey = "" } = useParams();
  const { settings, save, clear, saving, saveError, resetSaveError } = useSettings();

  const basic = rawKey ? undefined : findBasicSetting(id);
  const key = rawKey ? decodeURIComponent(rawKey) : (basic?.key ?? null);

  // Leaving the page should not carry a stale failure to the next one.
  useEffect(() => resetSaveError, [resetSaveError]);

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

          if (basic?.catalog) {
            return (
              <>
                <Heading name={title} desc={basic.desc} />
                <SaveError message={saveError?.message} />
                <CatalogDetail
                  basic={basic}
                  entry={entry}
                  provider={map.get("provider")?.value ?? ""}
                  model={map.get("model")?.value ?? ""}
                />
              </>
            );
          }

          return (
            <>
              <Heading name={title} desc={basic?.desc ?? entry.key} />
              <SaveError message={saveError?.message} />
              {basic?.options ? (
                <OptionPicker
                  options={basic.options}
                  value={entry.value}
                  disabled={saving}
                  onPick={(v) => v !== entry.value && void save(entry.key, v)}
                />
              ) : (
                <ValueEditor
                  initial={entry.value}
                  saving={saving}
                  onSave={(v) => void save(entry.key, v)}
                  onReset={entry.overridden ? () => void clear(entry.key) : undefined}
                />
              )}
              <div className="pt-3.5 text-[13px] leading-[1.6] text-pretty text-ink3">
                {basic?.options
                  ? "Changes apply straight away. Nothing to save."
                  : "Saved to the daemon. Invalid values are rejected with the reason above."}
              </div>
            </>
          );
        }}
      </DaemonState>
    </DetailShell>
  );
}
