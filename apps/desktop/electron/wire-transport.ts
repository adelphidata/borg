import { BorgError, type BorgTransport } from "@borg/client";
import type { WireTransport } from "./channels";

/**
 * The renderer's side of the bridge: rebuilds a `BorgTransport` from the
 * preload's error-as-data `WireTransport`.
 *
 * It must run in the renderer, not the preload. Only here is the `BorgError`
 * the app checks with `instanceof` the same class, and only here does the
 * status code survive — see `WireTransport` for why.
 */
export function fromWire(wire: WireTransport): BorgTransport {
  return {
    async unary(method, request, signal) {
      signal?.throwIfAborted();
      const reply = await wire.unary(method, request);
      if (!reply.ok) throw new BorgError(reply.error.code, reply.error.message, method);
      return reply.value as never;
    },

    serverStream(method, request, handlers) {
      return wire.serverStream(method, request, (event) => {
        switch (event.kind) {
          case "message":
            handlers.onMessage(event.message as never);
            break;
          case "error":
            handlers.onError?.(new BorgError(event.error.code, event.error.message, method));
            break;
          case "complete":
            handlers.onComplete?.();
            break;
        }
      });
    },
  };
}
