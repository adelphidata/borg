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
import { sessionsVersion } from "./sessionsVersion";
import {
  SESSIONS_REFRESH_DEBOUNCE_MS,
  SESSIONS_REFRESH_MAX_WAIT_MS,
  useSessionActivityFeed,
} from "./useSessionActivity";

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

function Probe({ listed }: { listed?: Set<string> }) {
  useSessionActivityFeed(listed && ((id) => listed.has(id)));
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

  const mount = async (listed?: Set<string>) => {
    const result = render(
      <PlatformProvider
        platform={{ kind: "web", transport: harness.transport, titleBarDrag: false }}
      >
        <Probe listed={listed} />
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

  describe("renewing the session lists", () => {
    const emit = async (sessionId: string, turnActive: boolean) => {
      await act(async () => {
        harness.subscriptions.at(-1)?.onMessage?.({ sessionId, turnActive });
      });
    };
    const settle = () => act(() => vi.advanceTimersByTime(SESSIONS_REFRESH_DEBOUNCE_MS + 1));

    it("renews the lists once a turn ends, after a short wait", async () => {
      // The unread dot reads from the listing's figures, and until now the
      // listing only re-read itself on navigation: a reply that landed in the
      // background showed nothing until the user happened to click somewhere.
      await mount(new Set(["s1"]));
      const before = sessionsVersion();
      await emit("s1", false);
      expect(sessionsVersion()).toBe(before);
      await settle();
      expect(sessionsVersion()).toBe(before + 1);
    });

    it("renews the lists for a conversation none of them has", async () => {
      // A schedule opening a conversation is a new row nobody clicked for.
      await mount(new Set(["s1"]));
      const before = sessionsVersion();
      await emit("from-a-schedule", true);
      await settle();
      expect(sessionsVersion()).toBe(before + 1);
    });

    it("leaves the lists alone when a listed conversation starts a turn", async () => {
      // The working dot comes from the activity store, not the listing; a
      // re-read here would be a listing per turn for nothing.
      await mount(new Set(["s1"]));
      const before = sessionsVersion();
      await emit("s1", true);
      await settle();
      expect(sessionsVersion()).toBe(before);
    });

    it("folds a burst of settles into one renewal", async () => {
      await mount(new Set(["a", "b", "c"]));
      const before = sessionsVersion();
      for (const id of ["a", "b", "c"]) {
        await emit(id, false);
        await act(() => vi.advanceTimersByTime(SESSIONS_REFRESH_DEBOUNCE_MS / 2));
      }
      await settle();
      expect(sessionsVersion()).toBe(before + 1);
    });

    it("renews within the maximum wait under a steady stream of settles", async () => {
      // Settles closer together than the debounce would otherwise put the
      // renewal off for as long as they keep coming.
      await mount(new Set(["a"]));
      const before = sessionsVersion();
      const step = SESSIONS_REFRESH_DEBOUNCE_MS / 2;
      let waited = 0;
      while (sessionsVersion() === before && waited <= SESSIONS_REFRESH_MAX_WAIT_MS * 2) {
        await emit("a", false);
        await act(() => vi.advanceTimersByTime(step));
        waited += step;
      }
      expect(sessionsVersion()).toBe(before + 1);
      expect(waited).toBeLessThanOrEqual(SESSIONS_REFRESH_MAX_WAIT_MS + step);
      expect(waited).toBeGreaterThanOrEqual(SESSIONS_REFRESH_MAX_WAIT_MS);
    });

    it("starts the maximum wait afresh after each renewal", async () => {
      // The deadline belongs to one pending renewal; carrying it over would
      // make the next one fire on the first event, with no debounce at all.
      await mount(new Set(["a"]));
      const before = sessionsVersion();
      await emit("a", false);
      await settle();
      expect(sessionsVersion()).toBe(before + 1);
      await act(() => vi.advanceTimersByTime(SESSIONS_REFRESH_MAX_WAIT_MS * 2));
      await emit("a", false);
      await act(() => vi.advanceTimersByTime(SESSIONS_REFRESH_DEBOUNCE_MS - 1));
      expect(sessionsVersion()).toBe(before + 1);
      await act(() => vi.advanceTimersByTime(2));
      expect(sessionsVersion()).toBe(before + 2);
    });

    it("renews once for a conversation the lists never return", async () => {
      // A session the listing never includes stays unlisted after its renewal,
      // so without a memory every event it sends would re-read the lists.
      await mount(new Set(["s1"]));
      const before = sessionsVersion();
      await emit("hidden", true);
      await settle();
      expect(sessionsVersion()).toBe(before + 1);
      for (const turnActive of [false, true, false]) {
        await emit("hidden", turnActive);
        await settle();
      }
      expect(sessionsVersion()).toBe(before + 1);
      // A different unknown id still gets its own renewal.
      await emit("another", true);
      await settle();
      expect(sessionsVersion()).toBe(before + 2);
    });

    it("drops a pending renewal when it unmounts", async () => {
      const { unmount } = await mount(new Set(["s1"]));
      const before = sessionsVersion();
      await emit("s1", false);
      unmount();
      await settle();
      expect(sessionsVersion()).toBe(before);
    });
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
