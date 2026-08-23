import { describe, expect, it } from "vitest";
import { createBorgClient } from "./client.js";
import { SERVICES, type ServiceName } from "./services.js";
import type { BorgTransport, StreamHandlers } from "./transport.js";

/**
 * A fake transport is the seam this package promises: feature code binds to
 * `BorgTransport` and nothing else. Every assertion here is about how the
 * client routes calls into that seam.
 */
interface Call {
  kind: "unary" | "serverStream";
  path: string;
  request: unknown;
  signal?: AbortSignal;
  handlers?: StreamHandlers<unknown>;
}

function recordingTransport() {
  const calls: Call[] = [];
  const unsubscribe = () => {};
  const transport: BorgTransport = {
    unary<Req, Res>(path: string, request: Req, signal?: AbortSignal): Promise<Res> {
      calls.push({ kind: "unary", path, request, signal });
      return Promise.resolve({ echoedFrom: path } as Res);
    },
    serverStream<Req, Res>(path: string, request: Req, handlers: StreamHandlers<Res>) {
      calls.push({
        kind: "serverStream",
        path,
        request,
        handlers: handlers as StreamHandlers<unknown>,
      });
      return unsubscribe;
    },
  };
  return { calls, transport, unsubscribe };
}

// Every RPC borgd exposes to a UI, derived from the same definitions the
// client binds — one row per method keeps this table honest as protos grow.
const ALL_METHODS = Object.entries(SERVICES).flatMap(([service, definition]) =>
  Object.entries(definition.methods).map(([key, method]) => ({
    service: service as ServiceName,
    key,
    path: `${definition.fullName}/${method.name}`,
    streaming: method.responseStream,
  })),
);

describe("createBorgClient", () => {
  it("exposes exactly the services in SERVICES", () => {
    const { transport } = recordingTransport();
    const client = createBorgClient(transport);
    expect(Object.keys(client).sort()).toEqual(Object.keys(SERVICES).sort());
  });

  it("binds every generated method key on each service", () => {
    const { transport } = recordingTransport();
    const client = createBorgClient(transport) as unknown as Record<
      string,
      Record<string, unknown>
    >;
    for (const [service, definition] of Object.entries(SERVICES)) {
      expect(Object.keys(client[service]).sort()).toEqual(Object.keys(definition.methods).sort());
    }
  });

  it("routes both streaming and unary RPCs, so the table exercises both branches", () => {
    expect(ALL_METHODS.some((m) => m.streaming)).toBe(true);
    expect(ALL_METHODS.some((m) => !m.streaming)).toBe(true);
  });

  it.each(ALL_METHODS)(
    "$service.$key hits the transport at $path",
    async ({ service, key, path, streaming }) => {
      const { calls, transport, unsubscribe } = recordingTransport();
      const client = createBorgClient(transport) as unknown as Record<
        string,
        Record<string, (...args: unknown[]) => unknown>
      >;
      const request = { probe: `${service}.${key}` };

      if (streaming) {
        const handlers = { onMessage: () => {} };
        const returned = client[service][key](request, handlers);
        expect(returned).toBe(unsubscribe);
        expect(calls).toEqual([{ kind: "serverStream", path, request, handlers }]);
      } else {
        const controller = new AbortController();
        const response = await client[service][key](request, controller.signal);
        // The transport's resolution comes back through the client unchanged.
        expect(response).toEqual({ echoedFrom: path });
        expect(calls).toEqual([{ kind: "unary", path, request, signal: controller.signal }]);
      }
    },
  );

  it("passes no signal when the caller gives none", async () => {
    const { calls, transport } = recordingTransport();
    const client = createBorgClient(transport);
    await client.status.getVitals({});
    expect(calls[0]?.signal).toBeUndefined();
  });
});
