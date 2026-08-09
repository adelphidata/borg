/**
 * The one seam between UI code and borgd.
 *
 * Every surface implements this and nothing else:
 *  - `apps/desktop` — Electron main holds a native gRPC channel over the unix
 *    socket; the renderer reaches it through a context-isolated IPC bridge.
 *  - `apps/web` — the browser speaks gRPC-Web to the gateway at `/grpc`.
 *
 * Feature code never imports a transport directly. It calls `createBorgClient`
 * and gets the same typed API on both, so a page written once runs everywhere.
 */

/** Fully-qualified proto method path, e.g. `borg.status.v1.Status/GetVitals`. */
export type MethodPath = string;

/** Cancels an in-flight server stream. Safe to call more than once. */
export type Unsubscribe = () => void;

export interface StreamHandlers<T> {
  onMessage: (message: T) => void;
  onError?: (error: Error) => void;
  /** Fires once when the server closes the stream cleanly. */
  onComplete?: () => void;
}

export interface BorgTransport {
  /** Unary call. Rejects with a `BorgError` on a non-OK gRPC status. */
  unary<Req, Res>(method: MethodPath, request: Req, signal?: AbortSignal): Promise<Res>;

  /**
   * Server-streaming call. Returns immediately; messages arrive on
   * `handlers.onMessage` until the stream ends or the returned function runs.
   */
  serverStream<Req, Res>(
    method: MethodPath,
    request: Req,
    handlers: StreamHandlers<Res>,
  ): Unsubscribe;
}

/** A gRPC failure carried across the IPC or HTTP boundary with its status intact. */
export class BorgError extends Error {
  constructor(
    /** Numeric gRPC status code (16 = UNAUTHENTICATED, 14 = UNAVAILABLE). */
    readonly code: number,
    message: string,
    readonly method?: MethodPath,
  ) {
    super(message);
    this.name = "BorgError";
  }

  /** True when borgd is not reachable — the UI shows "daemon offline" for this. */
  get isUnavailable(): boolean {
    return this.code === GrpcStatus.Unavailable;
  }

  /** True when the caller's token is missing, invalid, or revoked. */
  get isUnauthenticated(): boolean {
    return this.code === GrpcStatus.Unauthenticated;
  }
}

/** The subset of `grpc::Code` the UI branches on. */
export const GrpcStatus = {
  Ok: 0,
  Cancelled: 1,
  NotFound: 5,
  PermissionDenied: 7,
  Unimplemented: 12,
  Internal: 13,
  Unavailable: 14,
  Unauthenticated: 16,
} as const;
