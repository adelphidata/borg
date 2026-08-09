import { useEffect, useState } from "react";
import { Pill } from "@borg/ui";
import { useNative } from "../lib/platform";
import { useKeybind } from "../lib/keybinds";
import type { UpdateStatus } from "../lib/native";

/**
 * The only thing an update is allowed to do to the user.
 *
 * No modal, no forced restart, no progress bar competing with the transcript.
 * The download happens silently and this appears once a build is ready, in the
 * corner, until it is convenient. Everything else about updating is invisible.
 */
export function UpdatePill() {
  const native = useNative();
  const [status, setStatus] = useState<UpdateStatus>({ state: "idle" });
  const [dismissed, setDismissed] = useState(false);

  useEffect(() => native.updates.subscribe(setStatus), [native]);
  useKeybind("check-for-updates", () => {
    setDismissed(false);
    native.updates.check();
  });

  if (status.state !== "ready" || dismissed) return null;

  return (
    <div className="pointer-events-auto fixed right-5 bottom-5 z-40 flex items-center gap-2.5 rounded-[11px] border border-line bg-surface2 py-2 pr-2 pl-3.5 shadow-(--frameShadow)">
      <span className="text-[12.5px] leading-none text-ink2">
        Borg {status.version ?? ""} is ready
      </span>
      <Pill square variant="primary" onClick={() => native.updates.install()}>
        Restart
      </Pill>
      <button
        type="button"
        aria-label="Dismiss"
        onClick={() => setDismissed(true)}
        className="cursor-pointer border-0 bg-transparent px-1 text-[13px] leading-none text-ink3 hover:text-ink"
      >
        ×
      </button>
    </div>
  );
}
