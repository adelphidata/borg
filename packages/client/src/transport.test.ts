import { describe, expect, it } from "vitest";
import { BorgError, GrpcStatus } from "./transport.js";

describe("GrpcStatus", () => {
  // These are wire values from the gRPC spec; the Rust server produces the
  // numbers and the UI branches on the names, so a drifted value is a real bug.
  it.each([
    ["Ok", 0],
    ["Cancelled", 1],
    ["NotFound", 5],
    ["PermissionDenied", 7],
    ["Unimplemented", 12],
    ["Internal", 13],
    ["Unavailable", 14],
    ["Unauthenticated", 16],
  ] as const)("%s maps to spec code %d", (name, code) => {
    expect(GrpcStatus[name]).toBe(code);
  });
});

describe("BorgError", () => {
  it("carries code, message, and method across the boundary", () => {
    const error = new BorgError(
      GrpcStatus.NotFound,
      "no such session",
      "borg.session.v1.Session/Get",
    );
    expect(error).toBeInstanceOf(Error);
    expect(error.name).toBe("BorgError");
    expect(error.code).toBe(GrpcStatus.NotFound);
    expect(error.message).toBe("no such session");
    expect(error.method).toBe("borg.session.v1.Session/Get");
  });

  it("leaves method undefined when the failure has no call site", () => {
    expect(new BorgError(GrpcStatus.Internal, "boom").method).toBeUndefined();
  });

  // The UI's "daemon offline" and "enter token" branches hang off these two
  // getters, so each must fire only for its own status.
  it.each([
    [GrpcStatus.Unavailable, true, false],
    [GrpcStatus.Unauthenticated, false, true],
    [GrpcStatus.Internal, false, false],
    [GrpcStatus.Ok, false, false],
  ])("code %d → isUnavailable=%s isUnauthenticated=%s", (code, unavailable, unauthenticated) => {
    const error = new BorgError(code, "x");
    expect(error.isUnavailable).toBe(unavailable);
    expect(error.isUnauthenticated).toBe(unauthenticated);
  });
});
