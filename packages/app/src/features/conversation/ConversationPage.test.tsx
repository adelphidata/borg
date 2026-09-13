import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError, GrpcStatus, type session as sessionPb } from "@borg/client";
import { ThemeProvider } from "@borg/ui";
import { fakeDaemon, renderApp } from "../../test/harness";
import { SettingsProvider } from "../../lib/settings";
import { dispatchKeybind } from "../../lib/keybinds";
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

function mount(daemon: ReturnType<typeof fakeDaemon>, route = "/c/agent-1") {
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
    </Routes>,
    {
      daemon,
      route,
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

describe("ConversationPage", () => {
  it("names the conversation and the model it thinks with", async () => {
    mount(fakeDaemon(OPEN));
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());
    expect(screen.getByText("claude-opus-5")).toBeTruthy();
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
    // Long enough that the render budget hides the first half.
    const messages = Array.from({ length: 120 }, (_, i) => [
      { role: "user", text: `ask ${i}` },
      { role: "assistant", text: `answer ${i}` },
    ]).flat();
    mount(fakeDaemon({ ...OPEN, GetSession: { messages } }));
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

  it("says the connection dropped rather than freezing silently", async () => {
    // A silent stall used to look identical to an idle agent.
    const daemon = fakeDaemon(OPEN);
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Ship it")).toBeTruthy());

    const stream = daemon.streams.filter((s) => s.method === "Stream").at(-1)!;
    act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "socket closed")));
    await waitFor(() =>
      expect(screen.getByText("Lost the connection to Borg. Reconnecting…")).toBeTruthy(),
    );
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
