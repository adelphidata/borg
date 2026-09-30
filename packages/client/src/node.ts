import { createChannel, createClient, type Channel } from "nice-grpc";
import { indexClients, resolveMethod, toBorgError } from "./dispatch.js";
import { SERVICES, type ServiceName } from "./services.js";
import type { BorgTransport, MethodPath, StreamHandlers, Unsubscribe } from "./transport.js";

/**
 * Native gRPC over borgd's unix socket. Node only — Electron's main process.
 *
 * The socket is the daemon's primary transport and is authenticated by file
 * permissions (0600), so no token is needed here. The browser cannot reach it,
 * which is why `apps/web` uses `createWebTransport` instead.
 */
export interface NodeTransportOptions {
  /** Absolute path to `borgd.sock`. Defaults to `$BORG_HOME/borgd.sock`. */
  socketPath?: string;
}

/** Mirrors the daemon's own default (`~/.borg`), honouring a `BORG_HOME` override. */
export function defaultSocketPath(): string {
  const home = process.env.BORG_HOME ?? `${process.env.HOME ?? ""}/.borg`;
  return `${home}/borgd.sock`;
}

/**
 * Reconnect quickly once the daemon's socket is back.
 *
 * grpc-js backs a failed connection off 1.6× per attempt up to two minutes, a
 * policy meant for remote servers. While it waits, every call fails at once
 * with the last error, so a daemon restarted after a minute's outage looked
 * dead to the desktop app for another minute — and "Start Borg" appeared to
 * do nothing. Here the peer is a local socket and an attempt is one cheap
 * `connect(2)`.
 */
const SOCKET_CHANNEL_OPTIONS = {
  "grpc.initial_reconnect_backoff_ms": 250,
  "grpc.max_reconnect_backoff_ms": 2_000,
};

export function createNodeTransport(options: NodeTransportOptions = {}): BorgTransport & {
  channel: Channel;
  close(): void;
} {
  const socketPath = options.socketPath ?? defaultSocketPath();
  const channel = createChannel(`unix://${socketPath}`, undefined, SOCKET_CHANNEL_OPTIONS);

  // Every service is reachable over the socket, including bidi `capability`.
  const clients = indexClients(Object.keys(SERVICES) as ServiceName[], (definition) =>
    createClient(definition as never, channel),
  );

  return {
    channel,
    close: () => channel.close(),

    async unary<Req, Res>(path: MethodPath, request: Req, signal?: AbortSignal): Promise<Res> {
      const { client, key } = resolveMethod(clients, path);
      try {
        return (await client[key](request, { signal })) as Res;
      } catch (error) {
        throw toBorgError(error, path);
      }
    },

    serverStream<Req, Res>(
      path: MethodPath,
      request: Req,
      handlers: StreamHandlers<Res>,
    ): Unsubscribe {
      const { client, key } = resolveMethod(clients, path);
      const controller = new AbortController();

      void (async () => {
        try {
          const stream = client[key](request, {
            signal: controller.signal,
          }) as AsyncIterable<Res>;
          for await (const message of stream) handlers.onMessage(message);
          handlers.onComplete?.();
        } catch (error) {
          if (controller.signal.aborted) return; // caller unsubscribed; not an error
          handlers.onError?.(toBorgError(error, path));
        }
      })();

      return () => controller.abort();
    },
  };
}
