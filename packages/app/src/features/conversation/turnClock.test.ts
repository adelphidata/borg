import { describe, expect, it } from "vitest";
import { createTurnClock } from "./turnClock";

describe("createTurnClock", () => {
  it("keeps the submit time when the daemon announces the turn later", () => {
    const clock = createTurnClock();
    clock.start(1_000);
    clock.arm(1_800); // `preparing` arrives after the provider is reached

    expect(clock.stop(3_000)).toBe(2_000);
  });

  it("does not restart when a steer lands mid-turn", () => {
    const clock = createTurnClock();
    clock.start(1_000);
    clock.arm(2_500); // the steer

    expect(clock.stop(4_000)).toBe(3_000);
  });

  it("times a daemon-initiated turn from its first event", () => {
    const clock = createTurnClock();
    clock.arm(500);

    expect(clock.stop(2_500)).toBe(2_000);
  });

  it("reports nothing after a reset", () => {
    const clock = createTurnClock();
    clock.start(1_000);
    clock.reset();

    expect(clock.stop(5_000)).toBeUndefined();
    expect(clock.startedAt()).toBeUndefined();
  });

  it("reports nothing when no turn ran", () => {
    expect(createTurnClock().stop(5_000)).toBeUndefined();
  });

  it("spans the rewind round-trip on a resend", () => {
    const clock = createTurnClock();
    const submittedAt = 1_000;
    // The rewind RPC resolves at 1_400 and only then does the send begin.
    clock.start(submittedAt);
    clock.arm(1_400);

    expect(clock.stop(2_000)).toBe(1_000);
  });

  it("stops timing once a turn ends", () => {
    const clock = createTurnClock();
    clock.start(1_000);
    clock.stop(2_000);

    expect(clock.startedAt()).toBeUndefined();
    expect(clock.stop(3_000)).toBeUndefined();
  });
});
