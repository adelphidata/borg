import { useState } from "react";
import { Pill } from "@borg/ui";
import { onboardingSteps } from "../../data/mock";

/** First-run flow: three steps, one decision. Calls `onDone` after the last step. */
export function OnboardingPage({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState(0);
  const current = onboardingSteps[step];
  const isLast = step === onboardingSteps.length - 1;

  const advance = () => (isLast ? onDone() : setStep(step + 1));

  return (
    <div className="flex h-full items-center justify-center bg-surface [-webkit-app-region:drag]">
      <div className="flex min-h-[400px] w-[520px] max-w-[calc(100vw-48px)] flex-col px-[38px] pt-[34px] pb-[30px] [-webkit-app-region:no-drag]">
        <div className="mb-auto flex gap-1.5">
          {onboardingSteps.map((_, i) => (
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
        <div className="mt-[26px] flex items-center gap-3">
          <Pill variant="primary" sizeClasses="px-[22px] py-[11px] text-[13px]" onClick={advance}>
            {current.cta}
          </Pill>
          <span
            className="cursor-pointer text-[12.5px] leading-none text-ink3 hover:text-ink"
            onClick={advance}
          >
            {current.alt}
          </span>
        </div>
      </div>
    </div>
  );
}
