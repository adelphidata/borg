import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  BorgError,
  GrpcStatus,
  createBorgClient,
  type BorgClient,
  type StreamHandlers,
  type Unsubscribe,
} from "@borg/client";
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

function asBorgError(error: unknown): BorgError {
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

  useEffect(() => {
    let live = true;
    // Keep stale data on screen during a refetch; only a first load spins.
    setResult((prev) => (prev.state === "ready" ? prev : { state: "loading" }));
    setRefreshing(true);

    callRef.current(borg)
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
  error: BorgError | undefined;
  /** Clears a stored error, e.g. when the user edits the input again. */
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
  options: { onSuccess?: (data: T, vars: Vars) => void } = {},
): Mutation<Vars, T> {
  const borg = useBorg();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<BorgError>();
  const mutateRef = useRef(mutate);
  mutateRef.current = mutate;
  const onSuccessRef = useRef(options.onSuccess);
  onSuccessRef.current = options.onSuccess;

  const run = useCallback(
    async (vars: Vars): Promise<T | undefined> => {
      setPending(true);
      setError(undefined);
      try {
        const data = await mutateRef.current(borg, vars);
        onSuccessRef.current?.(data, vars);
        return data;
      } catch (e) {
        setError(asBorgError(e));
        return undefined;
      } finally {
        setPending(false);
      }
    },
    [borg],
  );

  const reset = useCallback(() => setError(undefined), []);

  return useMemo(() => ({ run, pending, error, reset }), [run, pending, error, reset]);
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
