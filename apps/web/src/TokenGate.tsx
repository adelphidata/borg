import { useState, type ReactNode } from "react";
import { Pill, STORAGE_KEYS, isSubmitEnter } from "@borg/ui";

/**
 * Sign-in for the browser surface.
 *
 * The desktop app needs no equivalent: it reaches borgd over the unix socket,
 * which the filesystem already guards. A browser has no such proof, so it
 * presents a client token that the gateway checks on every gRPC-Web call.
 *
 * `borg web` issues a token and opens `/ui/#/?token=…`; the fragment never
 * reaches the gateway, so the token is not in any access log. It is taken
 * from the address bar exactly once, stored, and stripped. Pasting a token
 * from `borg client-token issue <label>` remains the fallback.
 */

/** The token carried in the hash by `borg web`, if any; strips it once read. */
export function takeTokenFromHash(location: Location, history: History): string | null {
  const query = location.hash.split("?")[1];
  if (!query) return null;
  const params = new URLSearchParams(query);
  const token = params.get("token")?.trim();
  if (!token) return null;
  params.delete("token");
  const rest = params.toString();
  const path = location.hash.split("?")[0] || "#/";
  history.replaceState(
    history.state,
    "",
    `${location.pathname}${location.search}${path}${rest ? `?${rest}` : ""}`,
  );
  return token;
}

function initialToken(): string | null {
  const fromHash = takeTokenFromHash(window.location, window.history);
  if (fromHash) {
    localStorage.setItem(STORAGE_KEYS.token, fromHash);
    return fromHash;
  }
  return localStorage.getItem(STORAGE_KEYS.token);
}

export function TokenGate({ children }: { children: ReactNode }) {
  const [token, setToken] = useState(initialToken);
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
            Run <code className="text-ink">borg web</code> to open this page signed in, or paste a
            token from <code className="text-ink">borg client-token issue web</code>.
          </p>
        </div>

        <input
          type="password"
          value={draft}
          autoFocus
          placeholder="Client token"
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => isSubmitEnter(e) && save()}
          className="w-full rounded-[10px] border border-line2 bg-surface2 px-4 py-3 text-[13.5px] leading-none text-ink outline-none placeholder:text-ink3 focus:border-neon"
        />

        <Pill variant="primary" className="self-start" onClick={save}>
          Continue
        </Pill>
      </div>
    </div>
  );
}
