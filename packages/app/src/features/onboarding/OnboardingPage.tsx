import { useEffect, useState } from "react";
import { Pill } from "@borg/ui";
import { useMutation } from "../../lib/useDaemon";
import {
  detectionNote,
  leadDetection,
  modelsOf,
  providerLabel,
  providerOf,
  useSetupStatus,
  type ProviderChoice,
  type SetupStatus,
} from "./useSetupStatus";

/**
 * First run, on one screen.
 *
 * The daemon has already looked around (`Admin.GetSetupStatus`), so the
 * screen leads with whatever it found: a provider that is already configured,
 * or a key or a signed-in tool on this machine, needs one click. Only when
 * nothing is there does it ask — and then for exactly one thing, a key, with
 * the provider preselected and the model chosen. Everything else sits behind
 * "More providers" and "Change", so the default path is paste, click, talk.
 *
 * Every provider and model shown comes from the daemon; this file carries no
 * list of its own, which is what keeps it in step with the TUI wizard and the
 * settings pickers.
 */

const TITLE =
  "mx-0 mb-0 text-[30px] leading-[1.2] font-light tracking-[-0.03em] text-pretty text-ink";
const BODY = "mt-3.5 mb-0 text-[14.5px] leading-[1.62] text-pretty text-ink2";
const QUIET =
  "cursor-pointer border-0 bg-transparent p-0 text-[12.5px] leading-none text-ink3 hover:text-ink";
const PRIMARY = "px-[22px] py-[11px] text-[13px]";

function Card({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex h-full items-center justify-center bg-surface [-webkit-app-region:drag]">
      <div className="flex min-h-[400px] w-[520px] max-w-[calc(100vw-48px)] flex-col justify-end px-[38px] pt-[34px] pb-[30px] [-webkit-app-region:no-drag]">
        {children}
      </div>
    </div>
  );
}

function Hello() {
  return <h1 className={TITLE}>Hello. I’m Borg.</h1>;
}

export function OnboardingPage({ onDone }: { onDone: () => void }) {
  const status = useSetupStatus();
  // "Use something else" leaves the found-it path for the picker.
  const [pickerForced, setPickerForced] = useState(false);

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

  const busy = save.pending || complete.pending;
  const finish = () => {
    if (!complete.pending) void complete.run(undefined);
  };
  const saveAndFinish = async (vars: { provider: string; apiKey: string; model: string }) => {
    if (busy) return;
    // A failed write must not be papered over by marking onboarding done.
    if ((await save.run(vars)) === true) finish();
  };

  const errors = (
    <>
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
    </>
  );

  if (status.state === "loading") return <Card>{null}</Card>;

  if (status.state === "error") {
    // No catalog means nothing to pick from; the way out is a retry or the
    // Connections page later — never a screen with no button.
    return (
      <Card>
        <Hello />
        <p className={BODY}>I could not reach the part of me that runs on this machine.</p>
        <p className="mt-3 mb-0 text-[12.5px] leading-[1.5] text-alert">{status.error.message}</p>
        <div className="mt-[26px] flex items-center gap-3">
          <Pill variant="primary" sizeClasses={PRIMARY} onClick={status.refetch}>
            Try again
          </Pill>
          <button type="button" className={QUIET} onClick={finish}>
            I’ll do it later
          </button>
        </div>
        {errors}
      </Card>
    );
  }

  const data = status.data;
  const lead = leadDetection(data);
  const showPicker = pickerForced || (!data.configured && lead === undefined);

  if (!showPicker && data.configured) {
    return (
      <Card>
        <Hello />
        <p className={BODY}>
          You’re already set up with {providerLabel(data.providers, data.provider)}. Ask me
          anything.
        </p>
        {errors}
        <div className="mt-[26px] flex items-center gap-3">
          <Pill variant="primary" sizeClasses={PRIMARY} disabled={busy} onClick={finish}>
            {busy ? "Saving…" : "Start talking"}
          </Pill>
          <button type="button" className={QUIET} onClick={() => setPickerForced(true)}>
            Use something else
          </button>
        </div>
      </Card>
    );
  }

  if (!showPicker && lead) {
    const choice = providerOf(data.providers, lead.provider);
    const model = choice?.defaultModel ?? "";
    return (
      <Card>
        <Hello />
        <p className={BODY}>{detectionNote(data.providers, lead)} Nothing to paste.</p>
        {errors}
        <div className="mt-[26px] flex items-center gap-3">
          <Pill
            variant="primary"
            sizeClasses={PRIMARY}
            disabled={busy}
            onClick={() => void saveAndFinish({ provider: lead.provider, apiKey: "", model })}
          >
            {busy ? "Saving…" : `Use ${providerLabel(data.providers, lead.provider)}`}
          </Pill>
          <button type="button" className={QUIET} onClick={() => setPickerForced(true)}>
            Use something else
          </button>
        </div>
      </Card>
    );
  }

  return (
    <Card>
      <Picker
        status={data}
        busy={busy}
        errors={errors}
        onStart={(vars) => void saveAndFinish(vars)}
        onLater={finish}
      />
    </Card>
  );
}

function Picker({
  status,
  busy,
  errors,
  onStart,
  onLater,
}: {
  status: SetupStatus;
  busy: boolean;
  errors: React.ReactNode;
  onStart: (vars: { provider: string; apiKey: string; model: string }) => void;
  onLater: () => void;
}) {
  const providers = status.providers;
  const recommended = providers.find((p) => p.recommended) ?? providers[0];
  const [providerId, setProviderId] = useState(recommended?.id ?? "");
  const [apiKey, setApiKey] = useState("");
  const [model, setModel] = useState(recommended?.defaultModel ?? "");
  const [showAll, setShowAll] = useState(false);
  const [changingModel, setChangingModel] = useState(false);

  const selected = providerOf(providers, providerId);
  // A provider outside the featured three stays visible once picked, even
  // after the fold closes, or the selection would vanish from the screen.
  const featured = providers.filter((p) => p.featured || p.id === providerId);
  const rest = providers.filter((p) => !featured.includes(p));
  const models = modelsOf(providers, providerId);
  const needsKey = selected?.needsApiKey ?? false;
  const ready = selected !== undefined && (!needsKey || apiKey.trim() !== "");

  // The daemon's catalog can arrive after a refetch; keep the model in step
  // with the provider it belongs to.
  useEffect(() => {
    setModel(selected?.defaultModel ?? "");
    setChangingModel(false);
  }, [selected]);

  const pick = (p: ProviderChoice) => {
    setProviderId(p.id);
    setApiKey("");
  };
  const start = () => ready && !busy && onStart({ provider: providerId, apiKey, model });

  const pills = (list: ProviderChoice[]) =>
    list.map((p) => (
      <Pill
        key={p.id}
        square
        variant={providerId === p.id ? "primary" : undefined}
        onClick={() => pick(p)}
      >
        {p.label}
        {p.recommended && (
          <span className="ml-1.5 text-[10px] tracking-[0.08em] uppercase opacity-70">
            Recommended
          </span>
        )}
      </Pill>
    ));

  return (
    <>
      <Hello />
      <p className={BODY}>Pick where I think. One key and we’re talking.</p>
      <div className="mt-5 flex flex-wrap gap-2">
        {pills(featured)}
        {rest.length > 0 && (
          <button type="button" className={QUIET} onClick={() => setShowAll((v) => !v)}>
            {showAll ? "Fewer providers" : "More providers"}
          </button>
        )}
      </div>
      {showAll && rest.length > 0 && <div className="mt-2 flex flex-wrap gap-2">{pills(rest)}</div>}

      {selected && needsKey && (
        <>
          <input
            autoFocus
            type="password"
            className="mt-4 w-full rounded-[9px] border border-line bg-surface2 px-3 py-2.5 text-[13px] text-ink outline-none select-text placeholder:text-ink3"
            placeholder={`${selected.label} API key`}
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && start()}
          />
          {/* The one step of setup nobody can finish unaided: where the key
              comes from. Opens in the system browser on both surfaces. */}
          {selected.keyUrl !== "" && (
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
      {selected && !needsKey && (
        <p className="mt-4 mb-0 text-[12.5px] leading-[1.5] text-ink3">
          Nothing to paste. {selected.blurb}
        </p>
      )}

      {selected && models.length > 0 && (
        <div className="mt-4 text-[12.5px] leading-none text-ink3">
          Model:{" "}
          <span className="text-ink2">{models.find((m) => m.id === model)?.label ?? model}</span>
          {models.length > 1 && (
            <>
              {" · "}
              <button type="button" className={QUIET} onClick={() => setChangingModel((v) => !v)}>
                {changingModel ? "Keep" : "Change"}
              </button>
            </>
          )}
        </div>
      )}
      {changingModel && (
        <div className="mt-3 flex flex-col gap-2">
          {models.map((choice) => (
            <button
              key={choice.id}
              type="button"
              onClick={() => {
                setModel(choice.id);
                setChangingModel(false);
              }}
              className={`flex cursor-pointer flex-col items-start rounded-[10px] border px-3.5 py-2.5 text-left ${
                model === choice.id ? "border-neon bg-nav-on-bg" : "border-line2 bg-transparent"
              }`}
            >
              <span className="text-[13.5px] leading-none text-ink">{choice.label}</span>
              {choice.note !== "" && (
                <span className="mt-1.5 text-[11.5px] leading-none text-ink3">{choice.note}</span>
              )}
            </button>
          ))}
        </div>
      )}

      {errors}
      <div className="mt-[26px] flex items-center gap-3">
        <Pill variant="primary" sizeClasses={PRIMARY} disabled={!ready || busy} onClick={start}>
          {busy ? "Saving…" : "Start talking"}
        </Pill>
        <button type="button" className={QUIET} onClick={onLater}>
          I’ll do it later
        </button>
      </div>
    </>
  );
}
