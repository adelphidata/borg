import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BorgError, GrpcStatus, type StreamHandlers } from "./transport.js";

/**
 * `nice-grpc-web` is replaced by a fake channel/client pair so these tests can
 * observe what the web transport *does* — which address it dials, which
 * metadata it attaches, how it maps failures — without a gateway. The fakes
 * echo nothing on their own; every assertion is about transport behaviour.
 */

interface RecordedCall {
  service: string;
  key: string;
  request: unknown;
  metadata: Map<string, string>;
  signal: AbortSignal;
}

const fake = vi.hoisted(() => {
  const state = {
    channels: [] as string[],
    createdFor: [] as string[],
    calls: [] as RecordedCall[],
    /** Behaviour of the next unary call: value to resolve or error to throw. */
    unaryResult: undefined as { resolve?: unknown; reject?: unknown } | undefined,
    /** Messages the next stream yields before completing. */
    streamMessages: [] as unknown[],
    streamHangsUntilAbort: false,
  };
  return state;
});

vi.mock("nice-grpc-web", () => {
  class FakeMetadata {
    store = new Map<string, string>();
    set(key: string, value: string) {
      this.store.set(key, value);
      return this;
    }
  }

  return {
    Metadata: FakeMetadata,
    createChannel: (address: string) => {
      fake.channels.push(address);
      return { address };
    },
    createClient: (definition: {
      fullName: string;
      methods: Record<string, { responseStream: boolean }>;
    }) => {
      fake.createdFor.push(definition.fullName);
      const client: Record<string, unknown> = {};
      for (const [key, method] of Object.entries(definition.methods)) {
        client[key] = (
          request: unknown,
          options: { metadata: FakeMetadata; signal: AbortSignal },
        ) => {
          fake.calls.push({
            service: definition.fullName,
            key,
            request,
            metadata: options.metadata.store,
            signal: options.signal,
          });
          if (method.responseStream) {
            return (async function* () {
              yield* fake.streamMessages;
              if (fake.streamHangsUntilAbort) {
                if (!options.signal.aborted) {
                  await new Promise<void>((resolve) => {
                    options.signal.addEventListener("abort", () => resolve(), { once: true });
                  });
                }
                // Real nice-grpc-web surfaces the abort as a thrown error.
                throw Object.assign(new Error("stream aborted"), { code: 1 });
              }
            })();
          }
          if (fake.unaryResult?.reject) return Promise.reject(fake.unaryResult.reject);
          return Promise.resolve(fake.unaryResult?.resolve);
        };
      }
      return client;
    },
  };
});

import { SERVICES, WEB_SERVICES } from "./services.js";
import { createWebTransport } from "./web.js";

beforeEach(() => {
  fake.channels.length = 0;
  fake.createdFor.length = 0;
  fake.calls.length = 0;
  fake.unaryResult = undefined;
  fake.streamMessages = [];
  fake.streamHangsUntilAbort = false;
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const VITALS = "borg.status.v1.Status/GetVitals";

describe("createWebTransport", () => {
  it("dials the gateway's /grpc mount, stripping a trailing slash", () => {
    createWebTransport({ baseUrl: "https://borg.local:8443/", getToken: () => null });
    expect(fake.channels).toEqual(["https://borg.local:8443/grpc"]);
  });

  it("defaults to the page origin — the SPA is served by the same gateway", () => {
    vi.stubGlobal("location", { origin: "http://127.0.0.1:7777" });
    createWebTransport({ getToken: () => null });
    expect(fake.channels).toEqual(["http://127.0.0.1:7777/grpc"]);
  });

  it("builds clients for the web services only — pki never gets a browser client", () => {
    createWebTransport({ baseUrl: "http://x", getToken: () => null });
    expect(fake.createdFor.sort()).toEqual(
      [...WEB_SERVICES].map((name) => SERVICES[name].fullName).sort(),
    );
    expect(fake.createdFor).not.toContain("borg.pki.v1.Pki");
  });

  it("rejects a pki call locally with UNIMPLEMENTED instead of hitting the wire", async () => {
    const t = createWebTransport({ baseUrl: "http://x", getToken: () => null });
    await expect(t.unary("borg.pki.v1.Pki/StartPairing", {})).rejects.toMatchObject({
      name: "BorgError",
      code: GrpcStatus.Unimplemented,
    });
    expect(fake.calls).toEqual([]);
  });

  it("attaches the current token as a Bearer header, re-read on every call", async () => {
    let token: string | null = "tok-1";
    const t = createWebTransport({ baseUrl: "http://x", getToken: () => token });
    fake.unaryResult = { resolve: { ok: true } };

    await t.unary(VITALS, {});
    token = "tok-2";
    await t.unary(VITALS, {});

    expect(fake.calls.map((c) => c.metadata.get("authorization"))).toEqual([
      "Bearer tok-1",
      "Bearer tok-2",
    ]);
  });

  it("sends no authorization header before the user has a token", async () => {
    const t = createWebTransport({ baseUrl: "http://x", getToken: () => null });
    fake.unaryResult = { resolve: {} };
    await t.unary(VITALS, {});
    expect(fake.calls[0]?.metadata.has("authorization")).toBe(false);
  });

  it("returns the unary response and forwards request and signal untouched", async () => {
    const t = createWebTransport({ baseUrl: "http://x", getToken: () => "t" });
    const response = { stability: 42 };
    fake.unaryResult = { resolve: response };
    const controller = new AbortController();
    const request = { since: 7 };

    await expect(t.unary(VITALS, request, controller.signal)).resolves.toBe(response);
    expect(fake.calls[0]?.key).toBe("getVitals");
    expect(fake.calls[0]?.request).toBe(request);
    expect(fake.calls[0]?.signal).toBe(controller.signal);
  });

  it("normalizes a unary failure into a BorgError carrying the method", async () => {
    const t = createWebTransport({ baseUrl: "http://x", getToken: () => "t" });
    fake.unaryResult = { reject: { code: 16, details: "token revoked" } };
    const error = (await t.unary(VITALS, {}).then(
      () => {
        throw new Error("expected rejection");
      },
      (e: BorgError) => e,
    )) as BorgError;
    expect(error).toBeInstanceOf(BorgError);
    expect(error.code).toBe(GrpcStatus.Unauthenticated);
    expect(error.message).toBe("token revoked");
    expect(error.method).toBe(VITALS);
  });

  it("delivers stream messages with auth metadata, then completes", async () => {
    const t = createWebTransport({ baseUrl: "http://x", getToken: () => "stream-tok" });
    fake.streamMessages = [{ n: 1 }, { n: 2 }];

    const received: unknown[] = [];
    const completed = new Promise<void>((resolve, reject) => {
      t.serverStream(
        "borg.telemetry.v1.Telemetry/StreamCostEvents",
        { filter: "all" },
        { onMessage: (m) => received.push(m), onComplete: resolve, onError: reject },
      );
    });
    await completed;

    expect(received).toEqual([{ n: 1 }, { n: 2 }]);
    expect(fake.calls[0]?.key).toBe("streamCostEvents");
    expect(fake.calls[0]?.metadata.get("authorization")).toBe("Bearer stream-tok");
  });

  it("swallows the abort raised by unsubscribing — cancellation is not an error", async () => {
    const t = createWebTransport({ baseUrl: "http://x", getToken: () => null });
    fake.streamMessages = [{ n: 1 }];
    fake.streamHangsUntilAbort = true;

    const received: unknown[] = [];
    let errored: Error | undefined;
    let completedCount = 0;
    let firstMessage!: () => void;
    const first = new Promise<void>((resolve) => (firstMessage = resolve));
    const handlers: StreamHandlers<unknown> = {
      onMessage: (m) => {
        received.push(m);
        firstMessage();
      },
      onError: (e) => (errored = e),
      onComplete: () => completedCount++,
    };
    const unsubscribe = t.serverStream(
      "borg.telemetry.v1.Telemetry/StreamCostEvents",
      {},
      handlers,
    );
    await first;
    unsubscribe();
    unsubscribe();
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(received).toEqual([{ n: 1 }]);
    expect(errored).toBeUndefined();
    expect(completedCount).toBe(0);
  });

  it("reports a mid-stream server failure through onError as a BorgError", async () => {
    const t = createWebTransport({ baseUrl: "http://x", getToken: () => null });
    fake.streamMessages = [];
    // A stream against a service key that throws: reuse unary rejection shape via
    // a hanging stream is not needed — make the generator throw immediately.
    fake.streamHangsUntilAbort = false;
    const boom = { code: 14, details: "gateway down" };
    fake.streamMessages = {
      // an iterable whose iteration throws, standing in for a dead wire
      [Symbol.iterator]() {
        throw boom;
      },
    } as never;

    const error = await new Promise<BorgError>((resolve) => {
      t.serverStream(
        "borg.telemetry.v1.Telemetry/StreamCostEvents",
        {},
        {
          onMessage: () => {},
          onError: (e) => resolve(e as BorgError),
        },
      );
    });
    expect(error).toBeInstanceOf(BorgError);
    expect(error.code).toBe(GrpcStatus.Unavailable);
    expect(error.message).toBe("gateway down");
  });
});
