import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  BorgError,
  GrpcStatus,
  type BorgTransport,
  type StreamHandlers,
  type Unsubscribe,
} from "@borg/client";
import { MAX_DELAY_MS, OUTAGE_ESCALATION_MS } from "./backoff";
import { PlatformProvider } from "./platform";
import { useRetryingServerStream } from "./useDaemon";

/**
 * A transport whose only job is to hand each subscription's handlers back to
 * the test, so a stream can be failed or fed on demand.
 */
function fakeTransport() {
  const subscriptions: StreamHandlers<unknown>[] = [];
  const transport: BorgTransport = {
    unary: () => Promise.reject(new BorgError(GrpcStatus.Internal, "not used here")),
    serverStream: (_method, _request, handlers): Unsubscribe => {
      subscriptions.push(handlers as StreamHandlers<unknown>);
      return () => {};
    },
  };
  return { transport, subscriptions };
}

function Probe({ onMessage = () => {} }: { onMessage?: () => void }) {
  const state = useRetryingServerStream<unknown>(
    (borg, handlers) => borg.session.stream({ sessionId: "s", sinceEventSeq: 0 }, handlers),
    onMessage,
  );
  return <div data-testid="state">{state.sustained ? "sustained" : state.status}</div>;
}

describe("useRetryingServerStream", () => {
  let harness: ReturnType<typeof fakeTransport>;

  beforeEach(() => {
    vi.useFakeTimers();
    harness = fakeTransport();
  });

  afterEach(() => {
    cleanup();
    vi.useRealTimers();
  });

  const mount = (onMessage?: () => void) =>
    render(
      <PlatformProvider
        platform={{ kind: "web", transport: harness.transport, titleBarDrag: false }}
      >
        <Probe onMessage={onMessage} />
      </PlatformProvider>,
    );

  /** Fail the newest subscription and let its retry timer fire. */
  const dropAndRetry = async () => {
    const handlers = harness.subscriptions.at(-1);
    await act(async () => {
      handlers?.onError?.(new BorgError(GrpcStatus.Unavailable, "gone"));
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(MAX_DELAY_MS + 1);
    });
  };

  it("resubscribes after the stream drops", async () => {
    mount();
    expect(harness.subscriptions).toHaveLength(1);
    await dropAndRetry();
    expect(harness.subscriptions).toHaveLength(2);
  });

  /** Fail the newest subscription without letting its retry timer fire. */
  const drop = async () => {
    await act(async () => {
      harness.subscriptions.at(-1)?.onError?.(new BorgError(GrpcStatus.Unavailable, "gone"));
    });
  };

  const advance = async (ms: number) => {
    await act(async () => {
      await vi.advanceTimersByTimeAsync(ms);
    });
  };

  it("backs off further each time the reconnect itself fails", async () => {
    // The regression this guards: `useServerStream` reports `open` the instant
    // it subscribes, so treating "open" as success would reset the window on
    // every cycle and redial a dead daemon at the shortest delay forever.
    // Draws sit at the top of each window so the growth is observable.
    vi.spyOn(Math, "random").mockReturnValue(0.99);
    mount();

    await drop();
    await advance(1_100);
    expect(harness.subscriptions).toHaveLength(2);

    await drop();
    await advance(1_100);
    expect(harness.subscriptions).toHaveLength(2);
    await advance(1_000);
    expect(harness.subscriptions).toHaveLength(3);

    await drop();
    await advance(2_200);
    expect(harness.subscriptions).toHaveLength(3);
    await advance(2_000);
    expect(harness.subscriptions).toHaveLength(4);
  });

  it("resets the window once data actually arrives", async () => {
    vi.spyOn(Math, "random").mockReturnValue(0.99);
    const onMessage = vi.fn();
    mount(onMessage);

    // Grow the window well past its first step.
    for (let i = 0; i < 4; i += 1) await dropAndRetry();

    await act(async () => {
      harness.subscriptions.at(-1)?.onMessage?.({});
    });
    expect(onMessage).toHaveBeenCalled();

    const before = harness.subscriptions.length;
    await drop();
    await advance(1_100);
    expect(harness.subscriptions.length).toBe(before + 1);
  });

  it("escalates on elapsed time rather than on a number of attempts", async () => {
    const { getByTestId } = mount();
    await act(async () => {
      harness.subscriptions.at(-1)?.onError?.(new BorgError(GrpcStatus.Unavailable, "gone"));
    });
    expect(getByTestId("state").textContent).toBe("error");

    await act(async () => {
      await vi.advanceTimersByTimeAsync(OUTAGE_ESCALATION_MS + 1_000);
    });
    expect(getByTestId("state").textContent).toBe("sustained");
  });

  it("treats a clean stream end as an outage too", async () => {
    // These subscriptions are unbounded feeds, so `onComplete` never means
    // "finished" — it means the daemon went away without an error frame. Left
    // as a terminal state, the UI would sit on stale data forever, showing no
    // notice and never reconnecting.
    const { getByTestId } = mount();
    await act(async () => {
      harness.subscriptions.at(-1)?.onComplete?.();
    });
    expect(getByTestId("state").textContent).toBe("closed");

    await advance(MAX_DELAY_MS + 1);
    expect(harness.subscriptions).toHaveLength(2);
  });

  it("clears the escalation when the connection comes back", async () => {
    const { getByTestId } = mount();
    await act(async () => {
      harness.subscriptions.at(-1)?.onError?.(new BorgError(GrpcStatus.Unavailable, "gone"));
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(OUTAGE_ESCALATION_MS + 1_000);
    });
    expect(getByTestId("state").textContent).toBe("sustained");

    await act(async () => {
      harness.subscriptions.at(-1)?.onMessage?.({});
    });
    expect(getByTestId("state").textContent).not.toBe("sustained");
  });
});
