import { describe, expect, it, vi } from "vitest";
import { indexClients, resolveMethod, toBorgError } from "./dispatch.js";
import { SERVICES } from "./services.js";
import { BorgError, GrpcStatus } from "./transport.js";

const PATH = "borg.status.v1.Status/GetVitals";

function statusClients(methods: Record<string, (r: unknown, o?: unknown) => unknown>) {
  return new Map([["borg.status.v1.Status", methods]]);
}

describe("resolveMethod", () => {
  it("maps a proto path to the camelCase key on the right client", () => {
    const getVitals = vi.fn();
    const { client, key } = resolveMethod(statusClients({ getVitals }), PATH);
    expect(key).toBe("getVitals");
    expect(client.getVitals).toBe(getVitals);
  });

  it("splits on the last slash so dotted package names survive", () => {
    const clients = new Map([["a/b.c", { doThing: vi.fn() }]]);
    expect(resolveMethod(clients, "a/b.c/DoThing").key).toBe("doThing");
  });

  it("rejects a service this surface never built with UNIMPLEMENTED", () => {
    const path = "borg.pki.v1.Pki/Pair";
    let caught: unknown;
    try {
      resolveMethod(statusClients({ getVitals: vi.fn() }), path);
    } catch (error) {
      caught = error;
    }
    const error = caught as BorgError;
    expect(error).toBeInstanceOf(BorgError);
    expect(error.code).toBe(GrpcStatus.Unimplemented);
    expect(error.message).toContain("borg.pki.v1.Pki");
    expect(error.method).toBe(path);
  });

  it("rejects an unknown method on a known service with UNIMPLEMENTED", () => {
    const path = "borg.status.v1.Status/NoSuchRpc";
    let caught: unknown;
    try {
      resolveMethod(statusClients({ getVitals: vi.fn() }), path);
    } catch (error) {
      caught = error;
    }
    const error = caught as BorgError;
    expect(error).toBeInstanceOf(BorgError);
    expect(error.code).toBe(GrpcStatus.Unimplemented);
    expect(error.message).toContain(path);
  });
});

describe("indexClients", () => {
  it("keys each created client by its proto full name", () => {
    const created: string[] = [];
    const clients = indexClients(["status", "telemetry"], (definition) => {
      created.push(definition.fullName);
      return { marker: () => definition.fullName };
    });

    expect(created).toEqual(["borg.status.v1.Status", "borg.telemetry.v1.Telemetry"]);
    expect(clients.get("borg.status.v1.Status")?.marker(undefined)).toBe("borg.status.v1.Status");
    expect(clients.get(SERVICES.telemetry.fullName)).toBeDefined();
    expect(clients.size).toBe(2);
  });
});

describe("toBorgError", () => {
  it("passes an existing BorgError through untouched", () => {
    const original = new BorgError(GrpcStatus.NotFound, "gone", PATH);
    expect(toBorgError(original, "other/Path")).toBe(original);
  });

  it.each([
    // [thrown, expected code, expected message]
    [{ code: 5, details: "session gone" }, 5, "session gone"],
    // details wins over message when both are present
    [{ code: 5, details: "details", message: "message" }, 5, "details"],
    [{ code: 7, message: "denied" }, 7, "denied"],
    [{ code: 13 }, 13, "unknown daemon error"],
    // no numeric code at all → the call never reached a service
    [{ message: "socket hangup" }, GrpcStatus.Unavailable, "socket hangup"],
    [{ code: "5", message: "stringly" }, GrpcStatus.Unavailable, "stringly"],
    ["not even an object", GrpcStatus.Unavailable, "unknown daemon error"],
    [null, GrpcStatus.Unavailable, "unknown daemon error"],
  ])("normalizes %o → code %d", (thrown, code, message) => {
    const error = toBorgError(thrown, PATH);
    expect(error).toBeInstanceOf(BorgError);
    expect(error.code).toBe(code);
    expect(error.message).toBe(message);
    expect(error.method).toBe(PATH);
  });

  // nice-grpc-web rewraps sub-gRPC failures as UNKNOWN (2) with the real status
  // stringified into the message; the UI needs the real one back.
  it.each([
    ["CANCELLED", GrpcStatus.Cancelled],
    ["NOT_FOUND", GrpcStatus.NotFound],
    ["PERMISSION_DENIED", GrpcStatus.PermissionDenied],
    ["UNIMPLEMENTED", GrpcStatus.Unimplemented],
    ["INTERNAL", GrpcStatus.Internal],
    ["UNAVAILABLE", GrpcStatus.Unavailable],
    ["UNAUTHENTICATED", GrpcStatus.Unauthenticated],
  ])("recovers %s from a rewrapped UNKNOWN", (name, expected) => {
    const thrown = { code: 2, message: `Transport error: ${PATH} ${name}: upstream said no` };
    expect(toBorgError(thrown, PATH).code).toBe(expected);
  });

  it("falls back to UNAVAILABLE when UNKNOWN carries no recoverable status", () => {
    expect(toBorgError({ code: 2, message: "fetch failed" }, PATH).code).toBe(
      GrpcStatus.Unavailable,
    );
  });

  it("ignores shouty words that are not gRPC status names", () => {
    // HTTP matches the all-caps shape but is not a status; taking it would
    // invent a code.
    expect(toBorgError({ code: 2, message: "HTTP 502 from proxy" }, PATH).code).toBe(
      GrpcStatus.Unavailable,
    );
  });

  it("does not rewrite a real non-UNKNOWN code even if the message names another status", () => {
    const error = toBorgError({ code: 5, message: "UNAVAILABLE mentioned in passing" }, PATH);
    expect(error.code).toBe(5);
  });
});
