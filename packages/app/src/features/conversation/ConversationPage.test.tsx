import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError, GrpcStatus, type session as sessionPb } from "@borg/client";
import { ThemeProvider } from "@borg/ui";
import { fakeDaemon, renderApp } from "../../test/harness";
import { SettingsProvider } from "../../lib/settings";
import { dispatchKeybind } from "../../lib/keybinds";
import { sessionsVersion } from "../sessions/sessionsVersion";
import { ConversationPage } from "./ConversationPage";

const OPEN = {
  GetSettings: { entries: [] },
  ListSkills: { skills: [] },
  ListSessions: { sessions: [{ id: "agent-1" }] },
  Open: { sessionId: "live-1", lastEventSeq: 0 },
  GetState: {
    title: "Ship it",
    llmModel: "claude-opus-5",
    agentSessionId: "agent-1",
    turnInFlight: false,
  },
  GetSession: { messages: [] },
  GetContextBreakdown: { components: [{ tokens: 10_000 }], contextWindow: 200_000 },
  ListAgents: { agents: [] },
  GetGoal: { present: false },
};

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(
  daemon: ReturnType<typeof fakeDaemon>,
  route = "/c/agent-1",
  options: { kind?: "desktop" | "web"; newIsConversation?: boolean } = {},
) {
  return renderApp(
    <Routes>
      <Route
        path="/c/:sessionId"
        element={
          <>
            <ConversationPage />
            <Where />
          </>
        }
      />
      {/* The app routes `/c/new` to the page itself; most tests stand in a
          marker so a navigation there is visible. */}
      {!options.newIsConversation && <Route path="/c/new" element={<Where />} />}
    </Routes>,
    {
      daemon,
      route,
      ...(options.kind ? { kind: options.kind } : {}),
      // The composer's slash menu reads the settings registry, and its theme
      // rows read the theme.
      wrapper: (children) => (
        <ThemeProvider>
          <SettingsProvider>{children}</SettingsProvider>
        </ThemeProvider>
      ),
    },
  );
}

const emit = (daemon: ReturnType<typeof fakeDaemon>, event: Partial<sessionPb.AgentEvent>) => {
  const stream = daemon.streams.filter((s) => s.method === "Stream").at(-1)!;
  act(() => stream.handlers.onMessage({ eventSeq: 1, ...event } as never));
};

const composer = () => screen.getByPlaceholderText("Ask Borg anything");

/** A transcript long enough that the render budget hides its first half. */
const LONG = Array.from({ length: 120 }, (_, i) => [
  { role: "user", text: `ask ${i}` },
  { role: "assistant", text: `answer ${i}` },
]).flat();

/** jsdom lays nothing out, so a scroller's height is whatever the test says. */
function measure(el: HTMLElement, scrollHeight: number, clientHeight: number) {
  Object.defineProperty(el, "scrollHeight", { value: scrollHeight, configurable: true });
  Object.defineProperty(el, "clientHeight", { value: clientHeight, configurable: true });
}

const scrollerOf = (el: HTMLElement) => el.closest(".overflow-y-auto") as HTMLElement;

const pageKey = (key: "PageUp" | "PageDown", target: Element = document.body) =>
  act(() => void fireEvent.keyDown(target, { key }));

afterEach(() => {
  vi.restoreAllMocks();
});

describe("ConversationPage", () => {
  it("names the conversation and the model it thinks with", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
    expect(screen.getByText("claude-opus-5")).toBeTruthy();
  });

  it("renames the conversation from a double-click on its title", async () => {
    const daemon = fakeDaemon({ ...OPEN, SetSessionTitle: {} });
    const before = sessionsVersion();
    mount(daemon);
    const title = await screen.findByRole("heading", { level: 1, name: "Ship it" });
    fireEvent.doubleClick(title);
    const field = screen.getByRole("textbox", { name: "Conversation name" });
    fireEvent.change(field, { target: { value: "  Ship it today " } });
    fireEvent.keyDown(field, { key: "Enter" });
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSessionTitle")?.request).toEqual({
        id: "agent-1",
        title: "Ship it today",
      }),
    );
    expect(await screen.findByRole("heading", { level: 1, name: "Ship it today" })).toBeTruthy();
    // The sidebar list is told to refresh.
    expect(sessionsVersion()).toBe(before + 1);
  });

  it("keeps the field open when the rename fails", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      SetSessionTitle: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    fireEvent.doubleClick(await screen.findByRole("heading", { level: 1, name: "Ship it" }));
    const field = screen.getByRole("textbox", { name: "Conversation name" });
    fireEvent.change(field, { target: { value: "Nope" } });
    fireEvent.keyDown(field, { key: "Enter" });
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "SetSessionTitle")).toBe(true),
    );
    await act(async () => {});
    expect(screen.getByRole("textbox", { name: "Conversation name" })).toBeTruthy();
    expect(screen.queryByRole("heading", { level: 1, name: "Nope" })).toBeNull();
  });

  it("shows the model the last turn ran on, not the one the open found", async () => {
    // The picker can change the model between turns; the heading followed
    // the open's answer and said the old one until a reload.
    let model = "claude-opus-5";
    const daemon = fakeDaemon({ ...OPEN, GetState: () => ({ ...OPEN.GetState, llmModel: model }) });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("claude-opus-5")).toBeTruthy());
    model = "deepseek/deepseek-v3.2";
    emit(daemon, { preparing: {} as never });
    emit(daemon, { turnComplete: {} as never });
    await waitFor(() => expect(screen.getByText("deepseek/deepseek-v3.2")).toBeTruthy());
  });

  it("invites a first message rather than showing a blank page", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() =>
      expect(screen.getByText("Nothing here yet. Ask Borg anything.")).toBeTruthy(),
    );
  });

  it("reports a conversation that could not be opened", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      Open: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Could not load the conversation.")).toBeTruthy());
  });

  it("renders the turn as it streams", async () => {
    const daemon = fakeDaemon(OPEN);
    mount(daemon);
    await waitFor(() =>
      expect(screen.getByText("Nothing here yet. Ask Borg anything.")).toBeTruthy(),
    );

    emit(daemon, { textDelta: { text: "working on it" } as never });
    await waitFor(() => expect(screen.getByText("working on it")).toBeTruthy());
  });

  it("reveals earlier history when the reader scrolls up into the top edge", async () => {
    mount(fakeDaemon({ ...OPEN, GetSession: { messages: LONG } }));
    // Mounting a long transcript is slow under coverage instrumentation.
    const button = await screen.findByText("Show earlier messages", {}, { timeout: 5_000 });
    // A short cell costs two budget units, so the opening page is the last
    // fifty exchanges and each reveal adds fifty more.
    expect(screen.queryByText("ask 40")).toBeNull();
    const scroller = button.closest(".overflow-y-auto") as HTMLElement;

    // Down first, then up into the edge: position alone is not a request.
    scroller.scrollTop = 400;
    fireEvent.scroll(scroller);
    expect(screen.queryByText("ask 40")).toBeNull();
    scroller.scrollTop = 40;
    fireEvent.scroll(scroller);

    await waitFor(() => expect(screen.getByText("ask 40")).toBeTruthy());
    // One page, not the whole transcript: the button is still there for the rest.
    expect(screen.queryByText("ask 10")).toBeNull();
    expect(screen.getByText("Show earlier messages")).toBeTruthy();
  });

  it("reveals earlier history on a wheel turned up at the very top", async () => {
    // At the top there is nowhere to scroll, so no scroll event ever comes.
    mount(fakeDaemon({ ...OPEN, GetSession: { messages: LONG } }));
    const button = await screen.findByText("Show earlier messages", {}, { timeout: 5_000 });
    const scroller = scrollerOf(button);
    scroller.scrollTop = 0;

    fireEvent.wheel(scroller, { deltaY: 4 });
    expect(screen.queryByText("ask 40")).toBeNull();
    fireEvent.wheel(scroller, { deltaY: -4 });
    await waitFor(() => expect(screen.getByText("ask 40")).toBeTruthy());
    expect(screen.queryByText("ask 10")).toBeNull();
  });

  it("pages the transcript from the composer, and reveals more at the top", async () => {
    mount(fakeDaemon({ ...OPEN, GetSession: { messages: LONG } }));
    const button = await screen.findByText("Show earlier messages", {}, { timeout: 5_000 });
    const scroller = scrollerOf(button);
    measure(scroller, 3000, 500);
    scroller.scrollTop = 1200;
    composer().focus();

    pageKey("PageUp", composer());
    expect(scroller.scrollTop).toBe(700);
    pageKey("PageDown", composer());
    expect(scroller.scrollTop).toBe(1200);

    scroller.scrollTop = 0;
    expect(screen.queryByText("ask 40")).toBeNull();
    pageKey("PageUp", composer());
    await waitFor(() => expect(screen.getByText("ask 40")).toBeTruthy());
  });

  it("lets the keyboard reach and page the transcript itself", async () => {
    // A transcript of plain text holds nothing focusable, so the scroller
    // must take focus itself or a keyboard user cannot scroll it.
    mount(fakeDaemon({ ...OPEN, GetSession: { messages: LONG } }));
    const button = await screen.findByText("Show earlier messages", {}, { timeout: 5_000 });
    const scroller = scrollerOf(button);
    expect(screen.getByRole("region", { name: "Transcript" })).toBe(scroller);
    expect(scroller.tabIndex).toBe(0);

    measure(scroller, 3000, 500);
    scroller.scrollTop = 1200;
    scroller.focus();
    pageKey("PageUp", scroller);
    expect(scroller.scrollTop).toBe(700);
  });

  it("leaves the page keys to the find input", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
    const scroller = scrollerOf(screen.getByText("Nothing here yet. Ask Borg anything."));
    measure(scroller, 3000, 500);
    scroller.scrollTop = 1000;

    act(() => void dispatchKeybind("find-in-conversation"));
    const input = screen.getByLabelText("Find in conversation");
    input.focus();
    pageKey("PageUp", input);
    expect(scroller.scrollTop).toBe(1000);
  });

  it("says the connection dropped rather than freezing silently", async () => {
    // A silent stall used to look identical to an idle agent.
    const daemon = fakeDaemon(OPEN);
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());

    const stream = daemon.streams.filter((s) => s.method === "Stream").at(-1)!;
    act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "socket closed")));
    await waitFor(() =>
      expect(screen.getAllByText("Lost the connection to Borg. Reconnecting…")).not.toHaveLength(0),
    );
    // The banner is announced, not only shown: it lands in a live region.
    const statuses = screen.getAllByRole("status").map((el) => el.textContent);
    expect(statuses).toContain("Lost the connection to Borg. Reconnecting…");
  });

  it("announces the work as it starts and stops, without the ticking clock", async () => {
    const daemon = fakeDaemon(OPEN);
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
    // Mounted from the start, and quiet: a region that appears with its text
    // is often not read out at all.
    const idle = screen.getAllByRole("status").map((el) => el.textContent);
    expect(idle).toEqual(["", ""]);

    emit(daemon, { preparing: {} as never });
    await waitFor(() =>
      expect(screen.getAllByRole("status").map((el) => el.textContent)).toContain(
        "Waiting for the model",
      ),
    );
    // The visible line carries the elapsed counter, and only the visible line.
    const visible = screen
      .getAllByText("Waiting for the model", { exact: false })
      .find((el) => el.getAttribute("role") !== "status")!;
    expect(visible.getAttribute("aria-hidden")).toBe("true");
    expect(visible.textContent).not.toBe("Waiting for the model");

    emit(daemon, { turnComplete: {} as never });
    await waitFor(() =>
      expect(screen.getAllByRole("status").map((el) => el.textContent)).toEqual(["", ""]),
    );
  });

  it("offers a way back when Borg is waiting on an answer out of view", async () => {
    const daemon = fakeDaemon(OPEN);
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
    const scroller = scrollerOf(screen.getByText("Nothing here yet. Ask Borg anything."));
    measure(scroller, 3000, 500);
    // Reading further up: nothing has arrived, so no way back is offered.
    scroller.scrollTop = 100;
    fireEvent.scroll(scroller);
    expect(screen.queryByText("Jump to latest")).toBeNull();

    emit(daemon, {
      userInputRequest: { promptId: "p1", questions: [] } as never,
    });
    const back = await screen.findByRole("button", { name: "Borg has a question" });
    act(() => back.click());
    expect(scroller.scrollTop).toBe(3000);
    expect(screen.queryByRole("button", { name: "Borg has a question" })).toBeNull();
  });

  it("offers a way out of a conversation that is gone", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      Open: new BorgError(GrpcStatus.NotFound, "no such session"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Could not load the conversation.")).toBeTruthy());

    act(() => screen.getByText("Try again").click());
    await waitFor(() => expect(daemon.calls.filter((c) => c.method === "Open")).toHaveLength(2));

    act(() => screen.getByText("Start a new conversation").click());
    await waitFor(() => expect(screen.getByTestId("path").textContent).toBe("/c/new"));
  });

  it.each([
    {
      name: "Borg down on desktop, where Start Borg retries",
      error: GrpcStatus.Unavailable,
      kind: "desktop" as const,
      want: [],
    },
    {
      name: "Borg down in a browser, where nothing else is offered",
      error: GrpcStatus.Unavailable,
      kind: "web" as const,
      want: ["Try again"],
    },
    {
      name: "a rejected token, where Sign in again is the remedy",
      error: GrpcStatus.Unauthenticated,
      kind: "web" as const,
      want: [],
    },
    {
      name: "any other failure",
      error: GrpcStatus.NotFound,
      kind: "desktop" as const,
      want: ["Start a new conversation", "Try again"],
    },
  ])("offers only the ways out that can work: $name", async ({ error, kind, want }) => {
    mount(fakeDaemon({ ...OPEN, Open: new BorgError(error, "failed") }), "/c/agent-1", { kind });
    await waitFor(() => expect(screen.getByText("Could not load the conversation.")).toBeTruthy());

    const offered = ["Start a new conversation", "Try again"].filter(
      (label) => screen.queryByText(label) !== null,
    );
    expect(offered).toEqual(want);
  });

  it("does not offer a new conversation from one that is already new", async () => {
    // It would navigate to the route already on screen and do nothing.
    mount(fakeDaemon({ ...OPEN, Open: new BorgError(GrpcStatus.Internal, "failed") }), "/c/new", {
      newIsConversation: true,
    });
    await waitFor(() => expect(screen.getByText("Could not load the conversation.")).toBeTruthy());
    expect(screen.queryByText("Start a new conversation")).toBeNull();
    expect(screen.getByText("Try again")).toBeTruthy();
  });

  it("does not reveal history on a wheel that scrolls a box inside the transcript", async () => {
    mount(fakeDaemon({ ...OPEN, GetSession: { messages: LONG } }));
    const button = await screen.findByText("Show earlier messages", {}, { timeout: 5_000 });
    const scroller = scrollerOf(button);
    scroller.scrollTop = 0;
    // A tool's output box, part-way down its own text.
    const box = document.createElement("div");
    screen.getByText("answer 119").appendChild(box);
    measure(box, 600, 300);
    Object.defineProperty(box, "scrollTop", { value: 100, configurable: true });

    fireEvent.wheel(box, { deltaY: -4 });
    // A pinch on the trackpad arrives as a wheel with ctrl held.
    fireEvent.wheel(scroller, { deltaY: -4, ctrlKey: true });
    await act(async () => {});
    expect(screen.queryByText("ask 40")).toBeNull();
  });

  it("leaves the page keys to a control outside the transcript", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
    const scroller = scrollerOf(screen.getByText("Nothing here yet. Ask Borg anything."));
    measure(scroller, 3000, 500);
    scroller.scrollTop = 1000;

    const toggle = screen.getByRole("button", { name: "Toggle activity panel" });
    toggle.focus();
    pageKey("PageUp", toggle);
    expect(scroller.scrollTop).toBe(1000);

    // From the page itself the transcript is what pages.
    toggle.blur();
    pageKey("PageUp");
    expect(scroller.scrollTop).toBe(500);
  });

  it("opens the activity panel on request and closes it again", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
    expect(screen.queryByText("Activity")).toBeNull();

    act(() => screen.getByRole("button", { name: "Toggle activity panel" }).click());
    await waitFor(() => expect(screen.getByText("Activity")).toBeTruthy());

    act(() => screen.getByRole("button", { name: "Hide activity" }).click());
    expect(screen.queryByText("Activity")).toBeNull();
  });

  it("toggles the activity panel from the keyboard too", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());

    act(() => void dispatchKeybind("toggle-activity"));
    await waitFor(() => expect(screen.getByText("Activity")).toBeTruthy());
  });

  it("opens the find bar on the keybind and closes it on Escape", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());

    act(() => void dispatchKeybind("find-in-conversation"));
    expect(screen.getByRole("search")).toBeTruthy();

    // Escape belongs to the find bar while it is open, not to the turn.
    act(() => void dispatchKeybind("cancel-turn"));
    expect(screen.queryByRole("search")).toBeNull();
  });

  it("shows the context meter once the window is filling up", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      GetContextBreakdown: { components: [{ tokens: 120_000 }], contextWindow: 200_000 },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("60% full")).toBeTruthy());
  });

  it("keeps the meter hidden while the window is mostly empty", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
    expect(screen.queryByText(/% full/)).toBeNull();
  });

  it("forks into the new conversation the daemon made", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      GetContextBreakdown: { components: [{ tokens: 120_000 }], contextWindow: 200_000 },
      Fork: { forkedAgentSessionId: "agent-2" },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Fork")).toBeTruthy());

    await act(async () => screen.getByText("Fork").click());
    await waitFor(() => expect(screen.getByTestId("path").textContent).toBe("/c/agent-2"));
  });

  it("compacts, and says so while it runs", async () => {
    let release!: () => void;
    const daemon = fakeDaemon({
      ...OPEN,
      GetContextBreakdown: { components: [{ tokens: 180_000 }], contextWindow: 200_000 },
      Compact: () => new Promise((resolve) => (release = () => resolve({}))),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Compact")).toBeTruthy());

    act(() => screen.getByText("Compact").click());
    await waitFor(() => expect(screen.getByText("Compacting…")).toBeTruthy());

    await act(async () => release());
    await waitFor(() => expect(screen.getByText("Compact")).toBeTruthy());
  });

  it("reports a failed turn where it happened, with a retry", async () => {
    const daemon = fakeDaemon(OPEN);
    mount(daemon);
    await waitFor(() =>
      expect(screen.getByText("Nothing here yet. Ask Borg anything.")).toBeTruthy(),
    );

    fireEvent.change(composer(), { target: { value: "do the thing" } });
    await act(async () => {
      fireEvent.keyDown(composer(), { key: "Enter" });
    });
    const send = daemon.streams.find((s) => s.method === "Send")!;
    act(() => send.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "connection lost")));

    await waitFor(() => expect(screen.getByText("Try again")).toBeTruthy());
    await act(async () => screen.getByText("Try again").click());
    expect(daemon.streams.filter((s) => s.method === "Send")).toHaveLength(2);
  });
});
