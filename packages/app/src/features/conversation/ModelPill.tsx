import { useEffect, useRef, useState } from "react";
import { CheckIcon } from "@borg/ui";
import { modelLabel, modelsOf, useSetupStatus } from "../onboarding/useSetupStatus";
import { THINKING_OPTIONS } from "../settings/catalog";
import { useKeybind } from "../../lib/keybinds";
import { useSettings } from "../../lib/settings";

const ITEM =
  "flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent px-3 py-[7px] text-left text-[12.5px] leading-none text-ink2 hover:bg-surface3 hover:text-ink";

const GROUP = "px-3 pt-2 pb-1 text-[10px] font-medium tracking-[0.16em] text-ink3 uppercase";

/**
 * Which model answers, and how hard it thinks — from the composer.
 *
 * Both live in Settings, which is the right home for them and the wrong place
 * to go mid-conversation: choosing the model is part of asking the question,
 * not part of configuring the app. This is the same two settings through the
 * same mutation, so the two surfaces cannot disagree.
 *
 * There is no search box and no full catalog here on purpose. The daemon's
 * curated per-provider shortlist is the honest extent of what we can offer;
 * anything else the provider accepts still goes in Settings, where the field
 * takes any id.
 */
export function ModelPill() {
  const { settings, save, saving, saveError, resetSaveError } = useSettings();
  const setup = useSetupStatus();
  // Until the catalog answers (or if it never does) the pill still names the
  // id; only the short labels and the model list wait on it.
  const catalog = setup.state === "ready" ? setup.data.providers : [];
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open]);

  // Through the registry, not a raw listener: an open menu takes Escape and
  // hands it back on close, so closing the menu cannot also cancel the turn.
  useKeybind("cancel-turn", () => setOpen(false), { enabled: open, inInputs: true });

  if (settings.state !== "ready") return null;
  const model = settings.data.get("model")?.value ?? "";
  const provider = settings.data.get("provider")?.value ?? "";
  const thinking = settings.data.get("llm.thinking")?.value ?? "";
  // Nothing configured yet means onboarding has not run; a pill naming no
  // model, opening a menu offering none, is worse than no pill.
  if (model === "") return null;

  const models = modelsOf(catalog, provider);

  // A rejected write would otherwise close the menu and leave the old model on
  // screen, so the pill would state a model the daemon is not using.
  const pick = (key: string, value: string) => {
    setOpen(false);
    void save(key, value);
  };

  const toggle = () => {
    if (!open) resetSaveError();
    setOpen((v) => !v);
  };

  return (
    <div ref={root} className="relative">
      <button
        type="button"
        aria-label="Model and thinking level"
        aria-expanded={open}
        disabled={saving}
        onClick={toggle}
        className={`cursor-pointer border-0 bg-transparent px-1 text-[11px] leading-none hover:text-ink2 disabled:cursor-default ${
          saveError ? "text-alert" : "text-ink3"
        }`}
      >
        {modelLabel(catalog, provider, model)}
        {thinking !== "" && thinking !== "off" && ` · ${thinking}`}
      </button>
      {saveError && (
        <div role="alert" className="absolute bottom-6 left-0 z-20 text-[11px] text-alert">
          Could not change the model. Try again.
        </div>
      )}

      {open && (
        <div className="absolute bottom-6 left-0 z-20 w-[210px] overflow-hidden rounded-[9px] border border-line bg-surface2 py-1 shadow-(--frameShadow)">
          {models.length > 0 && (
            <>
              <div className={GROUP}>Model</div>
              {models.map((choice) => (
                <button
                  key={choice.id}
                  type="button"
                  className={ITEM}
                  onClick={() => pick("model", choice.id)}
                >
                  <span className="w-[15px] flex-none">
                    {choice.id === model && <CheckIcon size={15} className="text-neon-ink" />}
                  </span>
                  <span className="min-w-0 flex-1 truncate">{choice.label}</span>
                </button>
              ))}
              <div className="my-1 h-px bg-line2" />
            </>
          )}
          <div className={GROUP}>Thinking</div>
          {THINKING_OPTIONS.map((option) => (
            <button
              key={option.value}
              type="button"
              className={ITEM}
              onClick={() => pick("llm.thinking", option.value)}
            >
              <span className="w-[15px] flex-none">
                {option.value === thinking && <CheckIcon size={15} className="text-neon-ink" />}
              </span>
              {/* The plain word, with the outcome behind it on hover — the
                  menu is too narrow to carry both lines per row. */}
              <span className="min-w-0 flex-1 truncate capitalize" title={option.desc}>
                {option.value}
              </span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
