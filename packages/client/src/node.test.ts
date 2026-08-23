import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer, ServerError, Status } from "nice-grpc";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { AdminDefinition } from "./gen/admin.js";
import { StatusDefinition, Vitals } from "./gen/status.js";
import { CostEvent, TelemetryDefinition } from "./gen/telemetry.js";
import { createNodeTransport, defaultSocketPath } from "./node.js";
import { BorgError, GrpcStatus, type StreamHandlers } from "./transport.js";

/**
 * Runs the node transport against a real nice-grpc server on a unix socket —
 * the exact wire Electron's main process uses. Only the services under test
 * are served; the rest exercise the not-reachable paths.
 */

/** Serves every method of a definition; unary stubs return an empty message. */
function stubImpl(
  definition: { methods: Record<string, { responseStream: boolean; responseType: unknown }> },
  overrides: Record<string, unknown>,
): Record<string, unknown> {
  const impl: Record<string, unknown> = {};
  for (const [key, method] of Object.entries(definition.methods)) {
    impl[key] = method.responseStream
      ? async function* () {}
      : async () => (method.responseType as { fromPartial(p: object): unknown }).fromPartial({});
  }
  return { ...impl, ...overrides };
}

const dir = mkdtempSync(join(tmpdir(), "borg-client-"));
const socketPath = join(dir, "borgd.sock");
const server = createServer();
let streamCancelled: (() => void) | undefined;

beforeAll(async () => {
  server.add(
    StatusDefinition,
    stubImpl(StatusDefinition, {
      getVitals: async () => Vitals.fromPartial({ stability: 88, focus: 61, stage: "Evolved" }),
      getPosture: async () => {
        throw new ServerError(Status.NOT_FOUND, "posture missing");
      },
    }) as never,
  );
  server.add(
    TelemetryDefinition,
    stubImpl(TelemetryDefinition, {
      streamCostEvents: async function* (_request: unknown, context: { signal: AbortSignal }) {
        yield CostEvent.fromPartial({ provider: "anthropic", costUsd: "0.01" });
        yield CostEvent.fromPartial({ provider: "openrouter", costUsd: "0.02" });
        // Then hold the stream open until the client cancels.
        await new Promise<void>((resolve) => {
          context.signal.addEventListener("abort", () => resolve(), { once: true });
          streamCancelled = resolve;
        });
      },
    }) as never,
  );
  // Un-overridden: every unary returns an empty message, every stream ends cleanly.
  server.add(AdminDefinition, stubImpl(AdminDefinition, {}) as never);
  await server.listen(`unix://${socketPath}`);
});

afterAll(async () => {
  streamCancelled?.();
  await server.shutdown();
  rmSync(dir, { recursive: true, force: true });
});

function transport() {
  return createNodeTransport({ socketPath });
}

/** Collects a server stream until `count` messages arrive. */
function collect(count: number) {
  const messages: CostEvent[] = [];
  const errors: Error[] = [];
  let resolveEnough: () => void;
  const enough = new Promise<void>((resolve) => (resolveEnough = resolve));
  const handlers: StreamHandlers<CostEvent> = {
    onMessage: (message) => {
      messages.push(message);
      if (messages.length >= count) resolveEnough();
    },
    onError: (error) => {
      errors.push(error);
      resolveEnough();
    },
  };
  return { messages, errors, handlers, enough };
}

describe("createNodeTransport", () => {
  it("carries a unary call and its decoded response over the socket", async () => {
    const t = transport();
    try {
      const vitals = await t.unary<object, Vitals>("borg.status.v1.Status/GetVitals", {});
      expect(vitals.stability).toBe(88);
      expect(vitals.focus).toBe(61);
      expect(vitals.stage).toBe("Evolved");
    } finally {
      t.close();
    }
  });

  it("surfaces a server error as a BorgError with its status intact", async () => {
    const t = transport();
    try {
      const rejection = t.unary("borg.status.v1.Status/GetPosture", {});
      await expect(rejection).rejects.toBeInstanceOf(BorgError);
      const error = (await rejection.catch((e: BorgError) => e)) as BorgError;
      expect(error.code).toBe(GrpcStatus.NotFound);
      expect(error.message).toContain("posture missing");
      expect(error.method).toBe("borg.status.v1.Status/GetPosture");
    } finally {
      t.close();
    }
  });

  it("rejects a path outside SERVICES without touching the wire", async () => {
    const t = transport();
    try {
      await expect(t.unary("borg.nope.v1.Nope/Anything", {})).rejects.toMatchObject({
        name: "BorgError",
        code: GrpcStatus.Unimplemented,
      });
    } finally {
      t.close();
    }
  });

  it("delivers server-stream messages in order, then stays silent after unsubscribe", async () => {
    const t = transport();
    try {
      const { messages, errors, handlers, enough } = collect(2);
      const unsubscribe = t.serverStream(
        "borg.telemetry.v1.Telemetry/StreamCostEvents",
        {},
        handlers,
      );
      await enough;

      expect(messages.map((m) => m.provider)).toEqual(["anthropic", "openrouter"]);
      expect(messages[0]?.costUsd).toBe("0.01");

      unsubscribe();
      unsubscribe(); // safe to call twice, per the seam contract
      await new Promise((resolve) => setTimeout(resolve, 50));
      // Cancellation is the caller's choice, never an error.
      expect(errors).toEqual([]);
      expect(messages).toHaveLength(2);
    } finally {
      t.close();
    }
  });

  it("reports a stream against an unserved service through onError", async () => {
    const t = transport();
    try {
      const { errors, handlers, enough } = collect(1);
      // session is in SERVICES (so resolveMethod finds it) but not added to this server.
      t.serverStream("borg.session.v1.Session/Stream", {}, handlers);
      await enough;
      expect(errors[0]).toBeInstanceOf(BorgError);
      expect((errors[0] as BorgError).code).toBe(GrpcStatus.Unimplemented);
    } finally {
      t.close();
    }
  });

  it("fires onComplete when the server ends the stream cleanly", async () => {
    const t = transport();
    try {
      // Every non-overridden streaming stub yields nothing and returns.
      const completed = new Promise<void>((resolve, reject) => {
        t.serverStream(
          "borg.admin.v1.Admin/SubscribeSessionActivity",
          {},
          { onMessage: () => {}, onComplete: resolve, onError: reject },
        );
      });
      await expect(completed).resolves.toBeUndefined();
    } finally {
      t.close();
    }
  });

  it("normalizes a call after close() into a BorgError", async () => {
    const t = transport();
    t.close();
    await expect(t.unary("borg.status.v1.Status/GetVitals", {})).rejects.toBeInstanceOf(BorgError);
  });
});

describe("defaultSocketPath", () => {
  it.each([
    // [BORG_HOME, HOME, expected]
    ["/custom/borg-home", "/Users/someone", "/custom/borg-home/borgd.sock"],
    [undefined, "/Users/someone", "/Users/someone/.borg/borgd.sock"],
    [undefined, undefined, "/.borg/borgd.sock"],
  ])("BORG_HOME=%s HOME=%s → %s", (borgHome, home, expected) => {
    vi.stubEnv("BORG_HOME", borgHome as string);
    vi.stubEnv("HOME", home as string);
    try {
      expect(defaultSocketPath()).toBe(expected);
    } finally {
      vi.unstubAllEnvs();
    }
  });

  it("is what the transport dials when no socketPath is given", () => {
    vi.stubEnv("BORG_HOME", dir);
    try {
      const t = createNodeTransport();
      // The channel targets the default socket; a wrong default would strand
      // Electron main on a socket borgd never opens.
      expect(t.channel.getTarget()).toContain(join(dir, "borgd.sock"));
      t.close();
    } finally {
      vi.unstubAllEnvs();
    }
  });
});
