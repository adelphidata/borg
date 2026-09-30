import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { createChannel, createClient, type Channel } from "nice-grpc";
import { indexClients, resolveMethod, toBorgError } from "./dispatch.js";
import { SERVICES, type ServiceName } from "./services.js";
import type { BorgTransport, MethodPath, StreamHandlers, Unsubscribe } from "./transport.js";

/**
 * Native gRPC over borgd's unix socket. Node only — Electron's main process.
 *
 * The socket is the daemon's primary transport and is authenticated by file
 * permissions (0600), so no token is needed here. On Windows the same file
 * names an owner-only named pipe instead (see `localTarget`). The browser
 * cannot reach either, which is why `apps/web` uses `createWebTransport`.
 */
export interface NodeTransportOptions {
  /** Absolute path to `borgd.sock`. Defaults to `$BORG_HOME/borgd.sock`. */
  socketPath?: string;
  /** The platform whose transport to use. Defaults to `process.platform`. */
  platform?: NodeJS.Platform;
}

/** Mirrors the daemon's own default (`~/.borg`), honouring a `BORG_HOME` override. */
export function defaultSocketPath(): string {
  // Windows sets no HOME; the daemon uses the profile directory there.
  const home = process.env.BORG_HOME ?? `${process.env.HOME ?? homedir()}/.borg`;
  return `${home}/borgd.sock`;
}

const PIPE_PREFIX = "\\\\.\\pipe\\borgd-";

/**
 * The address `net.connect` takes for borgd's local listener at `socketPath`.
 *
 * On Windows borgd listens on a named pipe and writes the pipe's name into the
 * socket file (`crates/protocols/src/pipe.rs`); Node opens a named pipe by
 * that name. Null until the daemon has written one.
 */
export function localAddress(
  socketPath: string,
  platform: NodeJS.Platform = process.platform,
  read: (file: string) => string = (file) => readFileSync(file, "utf8"),
): string | null {
  if (platform !== "win32") return socketPath;
  try {
    const name = read(socketPath).trim();
    return name.startsWith(PIPE_PREFIX) ? name : null;
  } catch {
    return null;
  }
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
  const platform = options.platform ?? process.platform;

  // The target is resolved per call because on Windows it lives in a file the
  // daemon may not have written yet — the app often starts before borgd. The
  // channel is rebuilt only when the target changes.
  let current: { target: string; channel: Channel; clients: ReturnType<typeof index> } | null =
    null;
  let closed = false;
  function index(channel: Channel) {
    // Every service is reachable over the socket, including bidi `capability`.
    return indexClients(Object.keys(SERVICES) as ServiceName[], (definition) =>
      createClient(definition as never, channel),
    );
  }
  function connection() {
    // With no pipe name yet, keep the last target, or dial the file path so
    // the call fails as UNAVAILABLE, exactly like a missing unix socket.
    const address = localAddress(socketPath, platform);
    // grpc-js hands a `unix:` path to `net.connect`, which opens a named pipe
    // by its `\\.\pipe\...` name just as it opens a socket by its path.
    const target =
      address === null
        ? (current?.target ?? `unix://${socketPath}`)
        : platform === "win32"
          ? `unix:${address}`
          : `unix://${address}`;
    if (current === null || (!closed && current.target !== target)) {
      current?.channel.close();
      const channel = createChannel(target, undefined, SOCKET_CHANNEL_OPTIONS);
      // After close(), calls still fail the way they do on a closed channel.
      if (closed) channel.close();
      current = { target, channel, clients: index(channel) };
    }
    return current;
  }
  const clientsNow = () => connection().clients;

  return {
    get channel() {
      return connection().channel;
    },
    close: () => {
      closed = true;
      current?.channel.close();
    },

    async unary<Req, Res>(path: MethodPath, request: Req, signal?: AbortSignal): Promise<Res> {
      const { client, key } = resolveMethod(clientsNow(), path);
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
      const { client, key } = resolveMethod(clientsNow(), path);
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
