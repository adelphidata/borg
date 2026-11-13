import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { MAX_DELAY_MS } from "../../lib/backoff";
import { fakeDaemon, renderApp } from "../../test/harness";
import { meetingsChanged, meetingsVersion, subscribeMeetings } from "./meetingsStore";
import { useMeetingEventsFeed, useMeetingsVersion } from "./useMeetingEvents";

let seen = -1;
function Probe({ enabled }: { enabled: boolean }) {
  useMeetingEventsFeed(enabled);
  seen = useMeetingsVersion();
  return null;
}

const feeds = (daemon: ReturnType<typeof fakeDaemon>) =>
  daemon.streams.filter((s) => s.method === "SubscribeMeetingEvents");

describe("meetingsStore", () => {
  it("tells every subscriber, and stops telling one that left", () => {
    const a = vi.fn();
    const b = vi.fn();
    const leave = subscribeMeetings(a);
    subscribeMeetings(b);
    const before = meetingsVersion();
    meetingsChanged();
    leave();
    meetingsChanged();
    expect(meetingsVersion()).toBe(before + 2);
    expect(a).toHaveBeenCalledTimes(1);
    expect(b).toHaveBeenCalledTimes(2);
  });
});

describe("useMeetingEventsFeed", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it("opens no stream while Meetings is off", () => {
    const daemon = fakeDaemon();
    renderApp(<Probe enabled={false} />, { daemon });
    expect(feeds(daemon)).toHaveLength(0);
  });

  it("bumps the shared counter on every event, so every view re-reads", () => {
    const daemon = fakeDaemon();
    renderApp(<Probe enabled />, { daemon });
    const [feed] = feeds(daemon);
    const start = seen;
    act(() => feed!.handlers.onMessage({ summaryReady: { id: "m1" } } as never));
    act(() => feed!.handlers.onMessage({ detectionEnded: {} } as never));
    expect(seen).toBe(start + 2);
  });

  it("subscribes again after the stream drops", () => {
    const daemon = fakeDaemon();
    renderApp(<Probe enabled />, { daemon });
    act(() =>
      feeds(daemon)[0]!.handlers.onError!(new BorgError(GrpcStatus.Unavailable, "gone") as never),
    );
    act(() => void vi.advanceTimersByTime(MAX_DELAY_MS));
    expect(feeds(daemon)).toHaveLength(2);
  });

  it("closes the stream when Meetings is turned off", () => {
    let turnOff = () => {};
    function Toggle() {
      const [on, setOn] = useState(true);
      turnOff = () => setOn(false);
      return <Probe enabled={on} />;
    }
    const daemon = fakeDaemon();
    renderApp(<Toggle />, { daemon });
    act(() => turnOff());
    expect(daemon.cancelled).toContain("SubscribeMeetingEvents");
  });
});
