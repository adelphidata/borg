import { createChannel, createClient, Metadata } from "nice-grpc-web";
import { indexClients, resolveMethod, toBorgError } from "./dispatch.js";
import { WEB_SERVICES } from "./services.js";
import type { BorgTransport, MethodPath, StreamHandlers, Unsubscribe } from "./transport.js";

/**
 * gRPC-Web to the gateway's `/grpc` mount. Browser only.
 *
 * The browser can speak neither the unix socket nor borgd's mTLS TCP port, so
 * the gateway re-exposes the same service structs over gRPC-Web and gates them
 * on a client token. Bidi RPCs are unavailable on this wire — see `WEB_SERVICES`.
 */
export interface WebTransportOptions {
  /** Gateway origin. Defaults to the page origin, which is where the SPA is served. */
  baseUrl?: string;
  /** Returns the current client token, or null before the user has entered one. */
  getToken: () => string | null;
}

export function createWebTransport(options: WebTransportOptions): BorgTransport {
  const baseUrl = (options.baseUrl ?? globalThis.location.origin).replace(/\/$/, "");
  const channel = createChannel(`${baseUrl}/grpc`);
  const clients = indexClients(WEB_SERVICES, (definition) =>
    createClient(definition as never, channel),
  );

  const auth = () => {
    const token = options.getToken();
    const metadata = new Metadata();
    if (token) metadata.set("authorization", `Bearer ${token}`);
    return metadata;
  };

  return {
    async unary<Req, Res>(path: MethodPath, request: Req, signal?: AbortSignal): Promise<Res> {
      const { client, key } = resolveMethod(clients, path);
      try {
        return (await client[key](request, { metadata: auth(), signal })) as Res;
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
            metadata: auth(),
            signal: controller.signal,
          }) as AsyncIterable<Res>;
          for await (const message of stream) handlers.onMessage(message);
          handlers.onComplete?.();
        } catch (error) {
          if (controller.signal.aborted) return;
          handlers.onError?.(toBorgError(error, path));
        }
      })();

      return () => controller.abort();
    },
  };
}
