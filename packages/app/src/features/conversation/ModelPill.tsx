import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { CheckIcon, ChevronDownIcon, useIsMobile } from "@borg/ui";
import { effectiveModel, modelLabel, modelsOf, useSetupStatus } from "../onboarding/useSetupStatus";
import { THINKING_OPTIONS, optionLabel } from "../settings/catalog";
import { capitalize } from "../../lib/format";
import { useKeybind } from "../../lib/keybinds";
import { useSettings } from "../../lib/settings";
import { focusComposer } from "./composerFocus";

const HEADING = "text-[10px] leading-none font-medium tracking-[0.16em] text-ink3 uppercase";

const MODEL_ROW =
  "flex w-full cursor-pointer items-center gap-2.5 border-0 bg-transparent px-3 py-[7px] text-left text-[12.5px] leading-none text-ink2 outline-none hover:bg-surface3 hover:text-ink focus-visible:bg-surface3 focus-visible:text-ink";

/** Every choice in the open menu, in reading order, for the arrow keys to walk. */
const ITEMS = '[role="menuitemradio"]';

/**
 * Which model answers, and how hard it thinks — in the composer, beside the
 * microphone.
 *
 * Both live in Settings, which is the right home for them and the wrong place
 * to go mid-conversation: choosing the model is part of asking the question,
 * not part of configuring the app. This is the same two settings through the
 * same mutation, so the two surfaces cannot disagree.
 *
 * The models are the current provider's shortlist from the daemon's catalog,
 * in its order, which puts the strongest and newest first. There is no search
 * box and no full catalog here on purpose: anything else the provider accepts
 * still goes in Settings, where the field takes any id.
 */
export function ModelPill() {
  const { settings, save, saving, saveError, resetSaveError } = useSettings();
  const setup = useSetupStatus();
  // Until the catalog answers (or if it never does) the pill still names the
  // id; only the short labels and the model list wait on it.
  const catalog = setup.state === "ready" ? setup.data.providers : [];
  const isMobile = useIsMobile();
  const [open, setOpen] = useState(false);
  // The menu was a detour from typing, so closing it goes back to the box —
  // otherwise focus stays on the pill and the next keystroke goes nowhere.
  // Not on a phone, where focus raises the keyboard over the conversation.
  const close = () => {
    setOpen(false);
    if (!isMobile) focusComposer();
  };
  const root = useRef<HTMLDivElement>(null);
  const menu = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open]);

  // Into the menu at the model in force, so the arrow keys start from where
  // the user is, and a long list opens scrolled to it.
  useEffect(() => {
    if (!open) return;
    const items = menu.current?.querySelectorAll<HTMLElement>(ITEMS);
    const current = menu.current?.querySelector<HTMLElement>(`${ITEMS}[aria-checked="true"]`);
    (current ?? items?.[0])?.focus();
  }, [open]);

  // Through the registry, not a raw listener: an open menu takes Escape and
  // hands it back on close, so closing the menu cannot also cancel the turn.
  useKeybind("cancel-turn", close, { enabled: open, inInputs: true });

  if (settings.state !== "ready") return null;
  const provider = settings.data.get("provider")?.value ?? "";
  const model = effectiveModel(catalog, provider, settings.data.get("model")?.value ?? "");
  const thinking = settings.data.get("llm.thinking")?.value ?? "";
  // No model and no provider default means onboarding has not run; a pill
  // naming no model, opening a menu offering none, is worse than no pill.
  if (model === "") return null;

  const listed = modelsOf(catalog, provider);
  // A model set by hand outside the shortlist is still the one in force, so it
  // heads the list rather than leaving nothing ticked.
  const models = listed.some((m) => m.id === model)
    ? listed
    : [{ id: model, label: model, note: "" }, ...listed];
  const level = THINKING_OPTIONS.find((o) => o.value === thinking);
  // "off" is the absence of a claim, not a badge worth carrying. On a phone
  // the box is too narrow to carry it beside the model; the menu still shows it.
  const effort =
    isMobile || thinking === "" || thinking === "off"
      ? ""
      : level
        ? optionLabel(level)
        : capitalize(thinking);

  // A rejected write would otherwise close the menu and leave the old model on
  // screen, so the pill would state a model the daemon is not using.
  const pick = (key: string, value: string) => {
    close();
    void save(key, value);
  };

  const toggle = () => {
    if (!open) resetSaveError();
    setOpen((v) => !v);
  };

  // One list for the arrow keys, models then levels, so the whole menu is
  // reachable without Tab and without knowing where one part ends.
  const onMenuKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const items = Array.from(menu.current?.querySelectorAll<HTMLElement>(ITEMS) ?? []);
    const at = items.indexOf(document.activeElement as HTMLElement);
    const step: Record<string, number> = {
      ArrowDown: 1,
      ArrowRight: 1,
      ArrowUp: -1,
      ArrowLeft: -1,
    };
    let next: number | undefined;
    if (e.key in step) next = (at + step[e.key]! + items.length) % items.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = items.length - 1;
    if (next === undefined || items.length === 0) return;
    e.preventDefault();
    items[next]!.focus();
  };

  return (
    <div ref={root} className="relative flex-none">
      <button
        type="button"
        aria-label="Model and thinking level"
        aria-haspopup="menu"
        aria-expanded={open}
        disabled={saving}
        onClick={toggle}
        className={`flex h-[29px] ${isMobile ? "max-w-[140px]" : "max-w-[220px]"} cursor-pointer items-center gap-1.5 rounded-[9px] border-0 px-2.5 text-[12px] leading-none hover:bg-surface3 disabled:cursor-default ${
          open ? "bg-surface3" : "bg-transparent"
        } ${saveError ? "text-alert" : "text-ink2 hover:text-ink"}`}
      >
        <span className="min-w-0 truncate">{modelLabel(catalog, provider, model)}</span>
        {effort && <span className="flex-none text-ink3">{effort}</span>}
        <ChevronDownIcon size={12} className="flex-none text-ink3" />
      </button>
      {saveError && (
        <div
          role="alert"
          className="absolute right-0 bottom-[calc(100%+6px)] z-20 text-[11px] whitespace-nowrap text-alert"
        >
          Could not change the model. Try again.
        </div>
      )}

      {open && (
        <div
          ref={menu}
          role="menu"
          aria-label="Model and thinking level"
          onKeyDown={onMenuKey}
          className="absolute right-0 bottom-[calc(100%+10px)] z-30 w-[280px] overflow-hidden rounded-[11px] border border-line bg-surface2 shadow-(--frameShadow)"
        >
          <div role="group" aria-label="Model">
            <div className={`${HEADING} px-3 pt-3 pb-1.5`}>Model</div>
            <div className="max-h-[min(340px,45vh)] overflow-y-auto pb-1.5">
              {models.map((choice) => {
                const on = choice.id === model;
                return (
                  <button
                    key={choice.id}
                    type="button"
                    role="menuitemradio"
                    aria-checked={on}
                    tabIndex={-1}
                    className={MODEL_ROW}
                    onClick={() => (on ? close() : pick("model", choice.id))}
                  >
                    <span className={`min-w-0 flex-1 truncate ${on ? "text-ink" : ""}`}>
                      {choice.label}
                    </span>
                    {choice.note && (
                      <span className="flex-none text-[11px] text-ink3">{choice.note}</span>
                    )}
                    <span className="flex w-[15px] flex-none justify-end">
                      {on && <CheckIcon size={14} className="text-neon-ink" />}
                    </span>
                  </button>
                );
              })}
            </div>
          </div>
          <div role="group" aria-label="Thinking" className="border-t border-line2 px-3 pt-3 pb-3">
            <div className={HEADING}>Thinking</div>
            <div className="mt-2 flex gap-0.5 rounded-[8px] border border-line2 p-0.5">
              {THINKING_OPTIONS.map((option) => {
                const on = option.value === thinking;
                return (
                  <button
                    key={option.value}
                    type="button"
                    role="menuitemradio"
                    aria-checked={on}
                    tabIndex={-1}
                    title={option.desc}
                    className={`flex-auto cursor-pointer rounded-[6px] border-0 px-1.5 py-1.5 text-[11.5px] leading-none outline-none focus-visible:ring-1 focus-visible:ring-focus ${
                      on
                        ? "bg-surface3 font-medium text-ink"
                        : "bg-transparent text-ink3 hover:text-ink2"
                    }`}
                    onClick={() => (on ? close() : pick("llm.thinking", option.value))}
                  >
                    {optionLabel(option)}
                  </button>
                );
              })}
            </div>
            {level?.desc && (
              <div className="mt-2 text-[11.5px] leading-[1.4] text-ink3">{level.desc}</div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
