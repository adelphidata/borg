import { useState } from "react";
import { Pill } from "@borg/ui";
import { useMutation } from "../../lib/useDaemon";
import { PROVIDER_CATALOG } from "../connections/useConnections";
import { defaultModelFor, modelsFor, needsApiKey } from "./models";

/**
 * First run.
 *
 * Three steps, and the middle one does real work: it stores an API key and
 * writes `provider` and `model` so the very next screen can hold a
 * conversation. The previous version promised "paste a key from any provider"
 * and collected nothing, which left every new install one silent dead end away
 * from working.
 *
 * Skipping is still first-class — the Connections page does the same job — but
 * the default path now finishes setup rather than describing it.
 */

const TITLE =
  "mx-0 mb-0 text-[30px] leading-[1.2] font-light tracking-[-0.03em] text-pretty text-ink";
const BODY = "mt-3.5 mb-0 text-[14.5px] leading-[1.62] text-pretty text-ink2";

export function OnboardingPage({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState(0);
  const [provider, setProvider] = useState<string>();
  const [apiKey, setApiKey] = useState("");
  const [model, setModel] = useState("");

  const complete = useMutation((borg) => borg.admin.setOnboardingComplete({}), {
    onSuccess: onDone,
  });

  const save = useMutation(
    async (borg, vars: { provider: string; apiKey: string; model: string }) => {
      // The key first: a provider selected with no credential behind it is the
      // state this whole screen exists to avoid.
      if (vars.apiKey.trim() !== "") {
        await borg.admin.setApiKey({ provider: vars.provider, literal: vars.apiKey.trim() });
      }
      await borg.admin.setSetting({ key: "provider", value: vars.provider });
      if (vars.model !== "") {
        await borg.admin.setSetting({ key: "model", value: vars.model });
      }
      // `useMutation.run` reports failure as `undefined`, so a void return
      // would be indistinguishable from an error and the flow would never
      // advance. Return something.
      return true as const;
    },
  );

  const finish = () => {
    if (!complete.pending) void complete.run(undefined);
  };

  const saveAndFinish = async () => {
    if (!provider) return;
    // A failed write must not be papered over by marking onboarding done.
    if ((await save.run({ provider, apiKey, model })) === true) finish();
  };

  const selected = PROVIDER_CATALOG.find((p) => p.name === provider);

  const steps = [
    () => (
      <>
        <h1 className={TITLE}>Hello. I’m Borg.</h1>
        <p className={BODY}>
          I run on this machine, not in someone else’s cloud. Everything I learn stays in a folder
          you own.
        </p>
        <div className="mt-[26px] flex items-center gap-3">
          <Pill
            variant="primary"
            sizeClasses="px-[22px] py-[11px] text-[13px]"
            onClick={() => setStep(1)}
          >
            Get started
          </Pill>
          <span className="text-[12.5px] leading-none text-ink3">Takes about a minute</span>
        </div>
      </>
    ),

    () => (
      <>
        <h1 className={TITLE}>Pick a brain.</h1>
        <p className={BODY}>Choose a provider you already have a key for, or run one locally.</p>
        <div className="mt-5 flex flex-wrap gap-2">
          {PROVIDER_CATALOG.map((p) => (
            <Pill
              key={p.name}
              square
              variant={provider === p.name ? "primary" : undefined}
              onClick={() => {
                setProvider(p.name);
                setModel(defaultModelFor(p.name));
                setApiKey("");
              }}
            >
              {p.label}
            </Pill>
          ))}
        </div>
        {provider && needsApiKey(provider) && (
          <>
            <input
              autoFocus
              type="password"
              className="mt-4 w-full rounded-[9px] border border-line bg-surface2 px-3 py-2.5 text-[13px] text-ink outline-none select-text placeholder:text-ink3"
              placeholder={`${selected?.label} API key`}
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && apiKey.trim() !== "" && setStep(2)}
            />
            {/* The one step of setup nobody can finish unaided: where the key
                comes from. Opens in the system browser on both surfaces. */}
            {selected?.keyUrl && (
              <a
                href={selected.keyUrl}
                target="_blank"
                rel="noreferrer"
                className="mt-2.5 inline-block text-[12.5px] leading-none text-ink3 no-underline hover:text-ink"
              >
                Get a key from {selected.label} ↗
              </a>
            )}
          </>
        )}
        {provider && !needsApiKey(provider) && (
          <p className="mt-4 mb-0 text-[12.5px] leading-[1.5] text-ink3">
            Nothing to paste — Borg will talk to the model running on this machine.
          </p>
        )}
        <div className="mt-[26px] flex items-center gap-3">
          <Pill
            variant="primary"
            sizeClasses="px-[22px] py-[11px] text-[13px]"
            disabled={!provider || (needsApiKey(provider) && apiKey.trim() === "")}
            onClick={() => setStep(2)}
          >
            Next
          </Pill>
          <span
            className="cursor-pointer text-[12.5px] leading-none text-ink3 hover:text-ink"
            onClick={finish}
          >
            I’ll do it later
          </span>
        </div>
      </>
    ),

    () => {
      const choices = provider ? modelsFor(provider) : [];
      return (
        <>
          <h1 className={TITLE}>Ready when you are.</h1>
          <p className={BODY}>
            {choices.length > 0
              ? "Pick a model. You can change it any time in Settings."
              : "Ask me anything. I’ll add tools and connections as we need them."}
          </p>
          {choices.length > 0 && (
            <div className="mt-5 flex flex-col gap-2">
              {choices.map((choice) => (
                <button
                  key={choice.id}
                  type="button"
                  onClick={() => setModel(choice.id)}
                  className={`flex cursor-pointer flex-col items-start rounded-[10px] border px-3.5 py-2.5 text-left ${
                    model === choice.id ? "border-neon bg-nav-on-bg" : "border-line2 bg-transparent"
                  }`}
                >
                  <span className="text-[13.5px] leading-none text-ink">{choice.label}</span>
                  <span className="mt-1.5 text-[11.5px] leading-none text-ink3">
                    {choice.blurb}
                  </span>
                </button>
              ))}
            </div>
          )}
          {save.error && (
            <p className="mt-3 mb-0 text-[12.5px] leading-[1.5] text-alert">
              Could not save that: {save.error.message}
            </p>
          )}
          {complete.error && (
            <p className="mt-3 mb-0 text-[12.5px] leading-[1.5] text-ink3">
              Could not tell the daemon onboarding is done: {complete.error.message}
            </p>
          )}
          <div className="mt-[26px] flex items-center gap-3">
            <Pill
              variant="primary"
              sizeClasses="px-[22px] py-[11px] text-[13px]"
              disabled={save.pending || complete.pending}
              onClick={() => void saveAndFinish()}
            >
              {save.pending || complete.pending ? "Saving…" : "Start talking"}
            </Pill>
          </div>
        </>
      );
    },
  ];

  return (
    <div className="flex h-full items-center justify-center bg-surface [-webkit-app-region:drag]">
      <div className="flex min-h-[400px] w-[520px] max-w-[calc(100vw-48px)] flex-col px-[38px] pt-[34px] pb-[30px] [-webkit-app-region:no-drag]">
        <div className="mb-auto flex gap-1.5">
          {steps.map((_, i) => (
            <span
              key={i}
              className={`h-0.5 w-[22px] rounded-[2px] ${i <= step ? "bg-neon" : "bg-line"}`}
            />
          ))}
        </div>
        <div className="mt-auto">{steps[step]!()}</div>
      </div>
    </div>
  );
}
