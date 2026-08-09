import { useState, type ReactNode } from "react";
import { Pill, STORAGE_KEYS } from "@borg/ui";

/**
 * Sign-in for the browser surface.
 *
 * The desktop app needs no equivalent: it reaches borgd over the unix socket,
 * which the filesystem already guards. A browser has no such proof, so it
 * presents a client token (`borg client-token issue <label>`) that the gateway
 * checks on every gRPC-Web call.
 */
export function TokenGate({ children }: { children: ReactNode }) {
  const [token, setToken] = useState(() => localStorage.getItem(STORAGE_KEYS.token));
  const [draft, setDraft] = useState("");

  if (token) return <>{children}</>;

  const save = () => {
    const value = draft.trim();
    if (!value) return;
    localStorage.setItem(STORAGE_KEYS.token, value);
    setToken(value);
  };

  return (
    <div className="flex h-full flex-col items-center justify-center gap-6 bg-surface px-6">
      <div className="flex w-full max-w-[420px] flex-col gap-5">
        <div>
          <h1 className="text-[26px] leading-tight font-semibold tracking-[-0.02em] text-ink">
            Sign in to Borg.
          </h1>
          <p className="mt-2 text-[13.5px] leading-[1.5] text-ink2">
            Paste a client token. Create one with{" "}
            <code className="text-ink">borg client-token issue web</code>.
          </p>
        </div>

        <input
          type="password"
          value={draft}
          autoFocus
          placeholder="Client token"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && save()}
          className="w-full rounded-[10px] border border-line2 bg-surface2 px-4 py-3 text-[13.5px] leading-none text-ink outline-none placeholder:text-ink3 focus:border-neon"
        />

        <Pill variant="primary" style={{ alignSelf: "flex-start" }} onClick={save}>
          Continue
        </Pill>
      </div>
    </div>
  );
}
