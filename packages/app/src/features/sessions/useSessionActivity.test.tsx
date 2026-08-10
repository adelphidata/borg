import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  BorgError,
  GrpcStatus,
  type BorgTransport,
  type StreamHandlers,
  type Unsubscribe,
} from "@borg/client";
import { MAX_DELAY_MS } from "../../lib/backoff";
import { PlatformProvider } from "../../lib/platform";
import { isSessionWorking, resetActivity } from "./activityStore";
import { useSessionActivityFeed } from "./useSessionActivity";

/** A transport that answers `ListSessions` from a mutable fixture. */
function fakeTransport(working: Set<string>) {
  const subscriptions: StreamHandlers<unknown>[] = [];
  let listings = 0;
  const transport: BorgTransport = {
    unary: (method) => {
      if (String(method).includes("ListSessions")) {
        listings += 1;
        return Promise.resolve({
          sessions: [...working].map((id) => ({ id, turnActive: true })),
        } as never);
      }
      return Promise.reject(new BorgError(GrpcStatus.Unimplemented, String(method)));
    },
    serverStream: (_method, _request, handlers): Unsubscribe => {
      subscriptions.push(handlers as StreamHandlers<unknown>);
      return () => {};
    },
  };
  return { transport, subscriptions, listings: () => listings };
}

function Probe() {
  useSessionActivityFeed();
  return null;
}

describe("useSessionActivityFeed", () => {
  let working: Set<string>;
  let harness: ReturnType<typeof fakeTransport>;

  beforeEach(() => {
    vi.useFakeTimers();
    resetActivity();
    working = new Set<string>();
    harness = fakeTransport(working);
  });

  afterEach(() => {
    cleanup();
    resetActivity();
    vi.useRealTimers();
  });

  const mount = async () => {
    const result = render(
      <PlatformProvider
        platform={{ kind: "web", transport: harness.transport, titleBarDrag: false }}
      >
        <Probe />
      </PlatformProvider>,
    );
    // Let the mount-time listing resolve.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    return result;
  };

  it("adopts the working set the daemon already had", async () => {
    working.add("already-running");
    await mount();
    expect(isSessionWorking("already-running")).toBe(true);
  });

  it("folds in transitions as they arrive", async () => {
    await mount();
    await act(async () => {
      harness.subscriptions.at(-1)?.onMessage?.({ sessionId: "s1", turnActive: true });
    });
    expect(isSessionWorking("s1")).toBe(true);

    await act(async () => {
      harness.subscriptions.at(-1)?.onMessage?.({ sessionId: "s1", turnActive: false });
    });
    expect(isSessionWorking("s1")).toBe(false);
  });

  it("clears a dot whose settle was published while the stream was down", async () => {
    // The whole reason the listing is re-read on reconnect: the transition
    // that would have cleared this dot went out to nobody.
    await mount();
    await act(async () => {
      harness.subscriptions.at(-1)?.onMessage?.({ sessionId: "gone-quiet", turnActive: true });
    });
    expect(isSessionWorking("gone-quiet")).toBe(true);

    await act(async () => {
      harness.subscriptions.at(-1)?.onError?.(new BorgError(GrpcStatus.Unavailable, "dropped"));
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(MAX_DELAY_MS + 1);
    });

    expect(harness.subscriptions.length).toBeGreaterThan(1);
    expect(isSessionWorking("gone-quiet")).toBe(false);
  });

  it("lights a session that started working while the stream was down", async () => {
    await mount();
    working.add("started-offline");

    await act(async () => {
      harness.subscriptions.at(-1)?.onError?.(new BorgError(GrpcStatus.Unavailable, "dropped"));
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(MAX_DELAY_MS + 1);
    });

    expect(isSessionWorking("started-offline")).toBe(true);
  });

  it("keeps a transition that landed while the listing was in flight", async () => {
    // The listing describes a moment already past when it arrives. A turn that
    // starts during that round trip is newer than the snapshot, so letting the
    // snapshot win un-lights it — and the settle that follows changes nothing,
    // because the store already reads "not working". The dot would stay dark
    // for the whole turn.
    let release: (() => void) | undefined;
    const slow: BorgTransport = {
      ...harness.transport,
      unary: () =>
        new Promise((resolve) => {
          release = () => resolve({ sessions: [] } as never);
        }),
    };
    harness = { ...harness, transport: slow };

    await mount();
    await act(async () => {
      harness.subscriptions
        .at(-1)
        ?.onMessage?.({ sessionId: "started-mid-listing", turnActive: true });
    });

    await act(async () => {
      release?.();
      await vi.advanceTimersByTimeAsync(0);
    });

    expect(isSessionWorking("started-mid-listing")).toBe(true);
  });

  it("reads one listing per subscription, not one per event", async () => {
    await mount();
    const afterMount = harness.listings();
    for (let i = 0; i < 5; i += 1) {
      await act(async () => {
        harness.subscriptions.at(-1)?.onMessage?.({ sessionId: `s${i}`, turnActive: true });
      });
    }
    expect(harness.listings()).toBe(afterMount);
  });
});
