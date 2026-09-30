import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  BorgError,
  GrpcStatus,
  createBorgClient,
  type BorgClient,
  type StreamHandlers,
  type Unsubscribe,
} from "@borg/client";
import { OUTAGE_ESCALATION_MS, retryDelayMs } from "./backoff";
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

/** Normalizes anything a call path throws into a `BorgError` the UI can branch on. */
export function asBorgError(error: unknown): BorgError {
  return error instanceof BorgError
    ? error
    : new BorgError(GrpcStatus.Internal, error instanceof Error ? error.message : String(error));
}

/** What a unary RPC looks like while it is in flight, and after. */
export type Query<T> =
  | { state: "loading"; data?: undefined; error?: undefined }
  | { state: "ready"; data: T; error?: undefined }
  | { state: "error"; data?: undefined; error: BorgError };

export type QueryResult<T> = Query<T> & {
  /** Re-runs the call. Already-rendered data stays visible while it refreshes. */
  refetch: () => void;
  /** True from `refetch` (or a deps change) until the new response lands. */
  refreshing: boolean;
};

/**
 * Runs one unary RPC and tracks its outcome.
 *
 * Identical on desktop and web: the surface's transport decides whether the
 * call travels over the unix socket or gRPC-Web, and pages never find out.
 *
 * `deps` controls re-fetching — pass a value that changes when the call should
 * run again (a route param). For user-triggered refreshes call `refetch()`.
 */
export function useQuery<T>(
  call: (borg: BorgClient) => Promise<T>,
  deps: unknown[] = [],
): QueryResult<T> {
  const borg = useBorg();
  const [result, setResult] = useState<Query<T>>({ state: "loading" });
  const [refreshing, setRefreshing] = useState(true);
  const [tick, setTick] = useState(0);
  // `call` is a fresh closure each render; the ref keeps the effect keyed on
  // `deps` alone while still running the latest closure.
  const callRef = useRef(call);
  callRef.current = call;
  // What the last run asked for. A retry of the same call keeps its failure on
  // screen until the answer lands; a new target must not show the old error.
  const lastTarget = useRef<unknown[]>(undefined);

  useEffect(() => {
    let live = true;
    const target = [borg, ...deps];
    const previous = lastTarget.current;
    lastTarget.current = target;
    const keepError =
      previous !== undefined &&
      previous.length === target.length &&
      target.every((value, i) => Object.is(value, previous[i]));
    // Keep stale data on screen during a refetch; only a first load spins.
    setResult((prev) =>
      prev.state === "ready" || (keepError && prev.state === "error") ? prev : { state: "loading" },
    );
    setRefreshing(true);

    callRef
      .current(borg)
      .then((data) => {
        if (!live) return;
        setResult({ state: "ready", data });
        setRefreshing(false);
      })
      .catch((error: unknown) => {
        if (!live) return;
        setResult({ state: "error", error: asBorgError(error) });
        setRefreshing(false);
      });

    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [borg, tick, ...deps]);

  const refetch = useCallback(() => setTick((t) => t + 1), []);

  return useMemo(() => ({ ...result, refetch, refreshing }), [result, refetch, refreshing]);
}

/** A write RPC: run it on demand, watch it settle. */
export interface Mutation<Vars, T> {
  /** Resolves with the response, or `undefined` when the call failed (see `error`). */
  run: (vars: Vars) => Promise<T | undefined>;
  pending: boolean;
  /** Response of the most recent successful run. */
  data: T | undefined;
  error: BorgError | undefined;
  /** Clears a stored error and result, e.g. when the user edits the input again. */
  reset: () => void;
}

/**
 * The one write path for daemon mutations.
 *
 * Failures land in `error` instead of throwing, so pages render them inline;
 * `onSuccess` is where callers refetch whatever the write invalidated.
 */
export function useMutation<Vars, T>(
  mutate: (borg: BorgClient, vars: Vars) => Promise<T>,
  options: {
    onSuccess?: (data: T, vars: Vars) => void;
    /** Told why a call failed, as it fails — for a toast, say. */
    onError?: (error: BorgError, vars: Vars) => void;
  } = {},
): Mutation<Vars, T> {
  const borg = useBorg();
  const [pending, setPending] = useState(false);
  const [data, setData] = useState<T>();
  const [error, setError] = useState<BorgError>();
  const mutateRef = useRef(mutate);
  mutateRef.current = mutate;
  const onSuccessRef = useRef(options.onSuccess);
  onSuccessRef.current = options.onSuccess;
  const onErrorRef = useRef(options.onError);
  onErrorRef.current = options.onError;

  const run = useCallback(
    async (vars: Vars): Promise<T | undefined> => {
      setPending(true);
      setError(undefined);
      try {
        const result = await mutateRef.current(borg, vars);
        setData(result);
        onSuccessRef.current?.(result, vars);
        return result;
      } catch (e) {
        const error = asBorgError(e);
        setError(error);
        onErrorRef.current?.(error, vars);
        return undefined;
      } finally {
        setPending(false);
      }
    },
    [borg],
  );

  const reset = useCallback(() => {
    setError(undefined);
    setData(undefined);
  }, []);

  return useMemo(() => ({ run, pending, data, error, reset }), [run, pending, data, error, reset]);
}

/** Where a mount-scoped server stream currently stands. */
export interface StreamState {
  status: "open" | "closed" | "error";
  error?: BorgError;
}

/**
 * Subscribes to a server-streaming RPC for the lifetime of the component.
 *
 * `onMessage` is kept on a ref, so a fresh closure per render does not
 * resubscribe — only `deps` (e.g. a session id) restart the stream.
 */
export function useServerStream<T>(
  subscribe: (borg: BorgClient, handlers: StreamHandlers<T>) => Unsubscribe,
  onMessage: (message: T) => void,
  deps: unknown[] = [],
): StreamState {
  const borg = useBorg();
  const [state, setState] = useState<StreamState>({ status: "open" });
  const onMessageRef = useRef(onMessage);
  onMessageRef.current = onMessage;
  const subscribeRef = useRef(subscribe);
  subscribeRef.current = subscribe;

  useEffect(() => {
    let live = true;
    setState({ status: "open" });
    const unsubscribe = subscribeRef.current(borg, {
      onMessage: (message) => onMessageRef.current(message),
      onError: (error) => {
        if (live) setState({ status: "error", error: asBorgError(error) });
      },
      onComplete: () => {
        if (live) setState({ status: "closed" });
      },
    });
    return () => {
      live = false;
      unsubscribe();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [borg, ...deps]);

  return state;
}

export type RetryingStreamState = StreamState & {
  /** True once the outage has lasted long enough to stop looking like a blip. */
  sustained: boolean;
};

/**
 * A [`useServerStream`] that resubscribes itself after a drop, with
 * full-jitter backoff (see `./backoff`), and at once when the window or the
 * network comes back.
 *
 * A subscribe function that returns a no-op — the usual way to wait for an id
 * — simply never errors, so the retry loop stays dormant until there is
 * something to subscribe to.
 */
export function useRetryingServerStream<T>(
  subscribe: (borg: BorgClient, handlers: StreamHandlers<T>) => Unsubscribe,
  onMessage: (message: T) => void,
  deps: unknown[] = [],
): RetryingStreamState {
  const [attempt, setAttempt] = useState(0);
  const [sustained, setSustained] = useState(false);
  /** Consecutive failures since the last received message; drives the delay. */
  const failures = useRef(0);
  /** Start of the current outage, for escalation. */
  const outageSince = useRef<number | undefined>(undefined);

  const state = useServerStream<T>(
    subscribe,
    (message) => {
      // Receiving data is the only proof the connection works, so it is what
      // resets the backoff. `status: "open"` is set the moment we subscribe,
      // so a stream that dies on every open would otherwise reset the delay
      // forever and redial at full speed.
      failures.current = 0;
      outageSince.current = undefined;
      setSustained(false);
      onMessage(message);
    },
    [...deps, attempt],
  );

  // A fresh subscription target is a fresh outage clock — carrying the last
  // conversation's failure count into this one would open with a long delay
  // and an escalated notice for a connection that was never tried.
  useEffect(() => {
    failures.current = 0;
    outageSince.current = undefined;
    setSustained(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps]);

  useEffect(() => {
    // These streams are unbounded, so a clean `onComplete` is an outage too:
    // the daemon went away without an error frame. Only `open` means healthy.
    if (state.status === "open") return;
    const startedAt = outageSince.current ?? Date.now();
    outageSince.current = startedAt;
    const redial = () => setAttempt((n) => n + 1);
    const retry = setTimeout(redial, retryDelayMs(failures.current));
    failures.current += 1;
    const untilEscalation = Math.max(0, OUTAGE_ESCALATION_MS - (Date.now() - startedAt));
    const escalate = setTimeout(() => setSustained(true), untilEscalation);
    // The window coming back to the front, or the network coming back at all,
    // is the moment the user looks for the connection — up to thirty seconds
    // of a long-backed-off timer is exactly when a frozen screen gets noticed.
    // Redial now; the failure count stays, so a daemon that is still down does
    // not get its backoff reset by a tab switch.
    const onVisible = () => {
      if (document.visibilityState === "visible") {
        clearTimeout(retry);
        redial();
      }
    };
    const onOnline = () => {
      clearTimeout(retry);
      redial();
    };
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("online", onOnline);
    return () => {
      clearTimeout(retry);
      clearTimeout(escalate);
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("online", onOnline);
    };
  }, [state.status, state.error]);

  return useMemo(() => ({ ...state, sustained }), [state, sustained]);
}
