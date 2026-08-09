import { useState } from "react";
import { Pill } from "@borg/ui";
import { useMutation } from "../../lib/useDaemon";

interface OnboardingStep {
  title: string;
  body: string;
  cta: string;
  alt: string;
}

const steps: OnboardingStep[] = [
  {
    title: "Hello. I’m Borg.",
    body: "I run on this machine, not in someone else’s cloud. Everything I learn stays in a folder you own.",
    cta: "Get started",
    alt: "Takes about a minute",
  },
  {
    title: "Pick a brain.",
    body: "Paste a key from any provider you already use, or point me at one running locally. You can do this on the Connections page whenever.",
    cta: "Next",
    alt: "I'll do it later",
  },
  {
    title: "Ready when you are.",
    body: "Ask me anything. I’ll add tools and connections as we need them — no setup up front.",
    cta: "Start talking",
    alt: "Start talking",
  },
];

/**
 * First-run flow. The completion flag lives in the daemon
 * (`Admin.SetOnboardingComplete`), so finishing here also finishes for the
 * TUI and any other surface.
 */
export function OnboardingPage({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState(0);
  const current = steps[step];
  const isLast = step === steps.length - 1;

  const complete = useMutation((borg) => borg.admin.setOnboardingComplete({}), {
    onSuccess: onDone,
  });

  const advance = () => {
    if (!isLast) {
      setStep(step + 1);
      return;
    }
    if (!complete.pending) void complete.run(undefined);
  };

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
        <h1 className="mx-0 mt-auto mb-0 text-[30px] leading-[1.2] font-light tracking-[-0.03em] text-pretty text-ink">
          {current.title}
        </h1>
        <p className="mt-3.5 mb-0 text-[14.5px] leading-[1.62] text-pretty text-ink2">
          {current.body}
        </p>
        {complete.error && (
          <p className="mt-3 mb-0 text-[12.5px] leading-[1.5] text-ink3">
            Could not tell the daemon onboarding is done: {complete.error.message}
          </p>
        )}
        <div className="mt-[26px] flex items-center gap-3">
          <Pill variant="primary" sizeClasses="px-[22px] py-[11px] text-[13px]" onClick={advance}>
            {complete.pending ? "Saving…" : current.cta}
          </Pill>
          {current.alt !== current.cta && (
            <span
              className="cursor-pointer text-[12.5px] leading-none text-ink3 hover:text-ink"
              onClick={advance}
            >
              {current.alt}
            </span>
          )}
        </div>
      </div>
    </div>
  );
}
