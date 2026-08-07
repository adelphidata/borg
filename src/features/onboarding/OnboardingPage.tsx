import { useState } from "react";
import { Pill } from "../../components/Pill";
import { onboardingSteps } from "../../data/mock";

/** First-run flow: three steps, one decision. Calls `onDone` after the last step. */
export function OnboardingPage({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState(0);
  const current = onboardingSteps[step];
  const isLast = step === onboardingSteps.length - 1;

  const advance = () => (isLast ? onDone() : setStep(step + 1));

  return (
    <div className="onboarding">
      <div className="onboarding-card">
        <div className="pips">
          {onboardingSteps.map((_, i) => (
            <span key={i} className={i <= step ? "on" : ""} />
          ))}
        </div>
        <h1>{current.title}</h1>
        <p>{current.body}</p>
        <div className="actions">
          <Pill variant="primary" className="cta" onClick={advance}>
            {current.cta}
          </Pill>
          <span className="alt" onClick={advance}>
            {current.alt}
          </span>
        </div>
      </div>
    </div>
  );
}
