import { useEffect, useMemo, useState } from "react";
import { BorgError, createBorgClient, type BorgClient } from "@borg/client";
import { useTransport } from "./platform";

/**
 * The typed borgd client for the current surface.
 *
 * Memoised on the transport, which is created once per surface, so every caller
 * shares one client instance.
 */
export function useBorg(): BorgClient {
  const transport = useTransport();
  return useMemo(() => createBorgClient(transport), [transport]);
}

/** What a unary RPC looks like while it is in flight, and after. */
export type Query<T> =
  | { state: "loading"; data?: undefined; error?: undefined }
  | { state: "ready"; data: T; error?: undefined }
  | { state: "error"; data?: undefined; error: BorgError };

/**
 * Runs one unary RPC and tracks its outcome.
 *
 * Identical on desktop and web: the surface's transport decides whether the
 * call travels over the unix socket or gRPC-Web, and pages never find out.
 *
 * `deps` controls re-fetching — pass a value that changes when the call should
 * run again (a refresh counter, a route param).
 */
export function useQuery<T>(
  call: (borg: BorgClient) => Promise<T>,
  deps: unknown[] = [],
): Query<T> {
  const borg = useBorg();
  const [result, setResult] = useState<Query<T>>({ state: "loading" });

  useEffect(() => {
    let live = true;
    setResult({ state: "loading" });

    call(borg)
      .then((data) => live && setResult({ state: "ready", data }))
      .catch((error: unknown) => {
        if (!live) return;
        setResult({
          state: "error",
          error:
            error instanceof BorgError
              ? error
              : new BorgError(13, error instanceof Error ? error.message : String(error)),
        });
      });

    return () => {
      live = false;
    };
    // `call` is a fresh closure each render; `deps` is the real trigger.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [borg, ...deps]);

  return result;
}
