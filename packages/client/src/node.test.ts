import { mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { createServer, ServerError, Status } from "nice-grpc";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { AdminDefinition } from "./gen/admin.js";
import { StatusDefinition, Vitals } from "./gen/status.js";
import { CostEvent, TelemetryDefinition } from "./gen/telemetry.js";
import { createNodeTransport, defaultSocketPath, localAddress } from "./node.js";
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

describe("localAddress", () => {
  const pipe = "\\\\.\\pipe\\borgd-0123abcd";

  it("is the socket path itself off Windows", () => {
    expect(localAddress("/h/.borg/borgd.sock", "darwin", () => pipe)).toBe("/h/.borg/borgd.sock");
  });

  it("is the pipe name borgd wrote into the socket file on Windows", () => {
    expect(localAddress("C:\\h\\borgd.sock", "win32", () => `${pipe}\n`)).toBe(pipe);
  });

  it("refuses a file that names some other pipe", () => {
    expect(localAddress("C:\\h\\borgd.sock", "win32", () => "\\\\.\\pipe\\evil")).toBeNull();
  });

  it("is null before the daemon has written the file", () => {
    const missing = join(mkdtempSync(join(tmpdir(), "borg-pipe-")), "borgd.sock");
    expect(localAddress(missing, "win32")).toBeNull();
  });

  it("fails a Windows call as unavailable until a pipe name exists", async () => {
    const missing = join(mkdtempSync(join(tmpdir(), "borg-pipe-")), "borgd.sock");
    const t = createNodeTransport({ socketPath: missing, platform: "win32" });
    try {
      await expect(t.unary("borg.status.v1.Status/GetVitals", {})).rejects.toMatchObject({
        code: GrpcStatus.Unavailable,
      });
    } finally {
      t.close();
    }
  });
});

describe("defaultSocketPath", () => {
  it.each([
    // [BORG_HOME, HOME, expected]
    ["/custom/borg-home", "/Users/someone", "/custom/borg-home/borgd.sock"],
    [undefined, "/Users/someone", "/Users/someone/.borg/borgd.sock"],
    // Windows sets no HOME; the profile directory stands in, as for borgd.
    [undefined, undefined, `${homedir()}/.borg/borgd.sock`],
  ])("BORG_HOME=%s HOME=%s → %s", (borgHome, home, expected) => {
    vi.stubEnv("BORG_HOME", borgHome as string);
    vi.stubEnv("HOME", home as string);
    try {
      expect(defaultSocketPath()).toBe(expected);
    } finally {
      vi.unstubAllEnvs();
    }
  });

  it("retries a daemon that is not answering every couple of seconds, not minutes", async () => {
    // A socket that accepts and drops every connection is a daemon that is not
    // serving: each drop is one reconnect attempt, so the gaps between them
    // are the channel's backoff. grpc-js's default grows it 1.6× per attempt
    // toward two minutes — tuned for remote servers, and the reason a
    // restarted daemon used to look dead to the desktop app for a minute or
    // more. In 8s the default (1s, ×1.6) makes about four attempts; ours
    // (250ms, capped at 2s) makes about eight.
    const restartDir = mkdtempSync(join(tmpdir(), "borg-backoff-"));
    const path = join(restartDir, "borgd.sock");
    const attempts: number[] = [];
    const t0 = Date.now();
    const dropper = net.createServer((conn) => {
      attempts.push(Date.now() - t0);
      conn.destroy();
    });
    await new Promise<void>((r) => dropper.listen(path, r));
    const t = createNodeTransport({ socketPath: path });
    try {
      while (Date.now() - t0 < 8_000) {
        await t.unary("borg.status.v1.Status/GetVitals", {}).catch(() => {});
        await new Promise((r) => setTimeout(r, 300));
      }
      const gaps = attempts.slice(1).map((at, i) => at - attempts[i]!);
      expect(attempts.length).toBeGreaterThan(5);
      expect(Math.max(...gaps)).toBeLessThan(3_000);
    } finally {
      t.close();
      await new Promise((r) => dropper.close(r));
      rmSync(restartDir, { recursive: true, force: true });
    }
  }, 20_000);

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
