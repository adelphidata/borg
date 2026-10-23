import { render, type RenderResult } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import type { ReactElement, ReactNode } from "react";
import {
  BorgError,
  GrpcStatus,
  type BorgTransport,
  type StreamHandlers,
  type Unsubscribe,
} from "@borg/client";
import { PlatformProvider, type Platform } from "../lib/platform";
import type { NativeBridge } from "../lib/native";

/**
 * One place to stand a feature up against a fake daemon.
 *
 * Every page and hook reaches borgd through the platform's transport, so a
 * transport that answers from a table is enough to drive a real render — no
 * component needs a seam of its own, and the tests exercise the same client
 * code the app ships.
 */

/**
 * gRPC codes the daemon returns that `GrpcStatus` deliberately does not name —
 * that map is the subset the UI *branches* on, and widening it for a test would
 * make it look like more of it is load-bearing than is.
 */
export const INVALID_ARGUMENT = 3;
export const FAILED_PRECONDITION = 9;

/** Answers keyed by the last path segment of the method, e.g. `ListSessions`. */
export type UnaryTable = Record<string, unknown | ((request: unknown) => unknown)>;

export interface FakeDaemon {
  transport: BorgTransport;
  /** Every unary call made, in order. */
  calls: { method: string; request: unknown }[];
  /** Open streams, by method name, with the handlers the caller registered. */
  streams: { method: string; request: unknown; handlers: StreamHandlers<never> }[];
  /** Unsubscribes called so far. */
  cancelled: string[];
  /** Replace or add an answer after the fact. */
  answer(method: string, value: unknown | ((request: unknown) => unknown)): void;
}

const shortName = (method: string) => method.split("/").pop() ?? method;

/**
 * A transport that answers from `table`.
 *
 * An entry may be a value, a function of the request, a promise, or an error to
 * throw. A method with no entry rejects as `UNIMPLEMENTED`, which is what makes
 * a page quietly calling an RPC the test never anticipated visible instead of
 * silent.
 */
export function fakeDaemon(table: UnaryTable = {}): FakeDaemon {
  const answers: UnaryTable = { ...table };
  const calls: FakeDaemon["calls"] = [];
  const streams: FakeDaemon["streams"] = [];
  const cancelled: string[] = [];

  const transport: BorgTransport = {
    async unary(method, request) {
      calls.push({ method: shortName(method), request });
      const answer = answers[shortName(method)];
      if (answer === undefined) {
        throw new BorgError(GrpcStatus.Unimplemented, `no fake answer for ${shortName(method)}`);
      }
      const value =
        typeof answer === "function" ? (answer as (r: unknown) => unknown)(request) : answer;
      if (value instanceof Error) throw value;
      return (await value) as never;
    },
    serverStream(method, request, handlers): Unsubscribe {
      const name = shortName(method);
      streams.push({ method: name, request, handlers: handlers as StreamHandlers<never> });
      return () => cancelled.push(name);
    },
  };

  return {
    transport,
    calls,
    streams,
    cancelled,
    answer(method, value) {
      answers[method] = value;
    },
  };
}

/** A native bridge that records what the app asked the shell to do. */
export function fakeNative(): NativeBridge & {
  notifications: { title: string; body: string }[];
  badges: number[];
  dictationHotkeys: string[];
  revealed: string[];
  hidden: number;
  emitMenuCommand(action: string): void;
  emitActivateSession(sessionId: string): void;
} {
  const menuHandlers: ((action: string) => void)[] = [];
  const activateHandlers: ((sessionId: string) => void)[] = [];
  const bridge = {
    notifications: [] as { title: string; body: string }[],
    badges: [] as number[],
    dictationHotkeys: [] as string[],
    revealed: [] as string[],
    hidden: 0,
    onMenuCommand(handler: (action: string) => void) {
      menuHandlers.push(handler);
      return () => menuHandlers.splice(menuHandlers.indexOf(handler), 1);
    },
    onActivateSession(handler: (sessionId: string) => void) {
      activateHandlers.push(handler);
      return () => activateHandlers.splice(activateHandlers.indexOf(handler), 1);
    },
    notify(notification: { title: string; body: string }) {
      bridge.notifications.push(notification);
    },
    revealSession(sessionId: string) {
      bridge.revealed.push(sessionId);
    },
    hideWindow() {
      bridge.hidden += 1;
    },
    setBadgeCount(count: number) {
      bridge.badges.push(count);
    },
    setDictationHotkey(accelerator: string) {
      bridge.dictationHotkeys.push(accelerator);
    },
    // Reports Borg as already up, the way a warm start does — otherwise every
    // test would render the boot screen instead of the app. A test about the
    // boot screen replaces this.
    boot: {
      subscribe: (handler: (status: unknown) => void) => {
        handler({ state: "ready", message: "Ready", progress: 1, canRetry: false });
        return () => {};
      },
      retry: async () => {},
    },
    updates: { subscribe: () => () => {}, check: () => {}, install: () => {} },
    emitMenuCommand(action: string) {
      for (const handler of [...menuHandlers]) handler(action);
    },
    emitActivateSession(sessionId: string) {
      for (const handler of [...activateHandlers]) handler(sessionId);
    },
  };
  return bridge as never;
}

export interface HarnessOptions {
  daemon?: FakeDaemon;
  native?: NativeBridge;
  kind?: Platform["kind"];
  /** Initial router entries; the first is the starting location. */
  route?: string;
  /** Extra providers to wrap the tree in, innermost last. */
  wrapper?: (children: ReactNode) => ReactElement;
}

export interface Harness extends RenderResult {
  daemon: FakeDaemon;
}

/** Render `ui` inside the providers every feature expects. */
export function renderApp(ui: ReactElement, options: HarnessOptions = {}): Harness {
  const daemon = options.daemon ?? fakeDaemon();
  const platform: Platform = {
    kind: options.kind ?? "desktop",
    transport: daemon.transport,
    titleBarDrag: false,
    native: options.native,
  };
  const inner = options.wrapper ? options.wrapper(ui) : ui;
  const view = render(
    <MemoryRouter initialEntries={[options.route ?? "/"]}>
      <PlatformProvider platform={platform}>{inner}</PlatformProvider>
    </MemoryRouter>,
  );
  return Object.assign(view, { daemon });
}
