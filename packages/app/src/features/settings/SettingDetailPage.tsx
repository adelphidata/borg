import { useEffect, useState, type ReactNode } from "react";
import { useParams } from "react-router-dom";
import {
  BackLink,
  CheckIcon,
  DETAIL_COL,
  PageChrome,
  Pill,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
  useIsMobile,
  useTheme,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useSettings } from "../../lib/settings";
import { findBasicSetting, type SettingOption } from "./catalog";

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
          <div
            key={o.value}
            className="flex cursor-pointer items-center gap-4 border-b border-line2 px-4 py-3.5 transition-[background] duration-150 last:border-b-0 hover:bg-surface3"
            onClick={() => onPick(o.value)}
          >
            <div className="min-w-0 flex-1">
              <div
                className={`text-[13.5px] leading-[1.3] font-medium ${
                  on ? "text-neon-ink" : "text-ink"
                }`}
              >
                {o.value}
              </div>
              {o.desc && (
                <div className="mt-[3px] text-[12px] leading-[1.45] text-ink3">{o.desc}</div>
              )}
            </div>
            {on && <CheckIcon size={15} role="img" aria-label="Selected" />}
          </div>
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
        onKeyDown={(e) => e.key === "Enter" && !saving && onSave(draft)}
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

/** The local-only theme setting — applies before paint, so it never touches borgd. */
function AppearanceDetail() {
  const { theme, toggleTheme } = useTheme();
  const value = theme === "light" ? "Light" : "Dark";
  const setting = findBasicSetting("appearance")!;

  return (
    <DetailShell title={setting.name}>
      <Heading name={setting.name} desc={setting.desc} />
      <OptionPicker
        options={setting.options ?? []}
        value={value}
        disabled={false}
        onPick={(v) => v !== value && toggleTheme()}
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
