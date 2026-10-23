import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
import { BorgError } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { SettingsProvider } from "../../lib/settings";
import { FailureCard } from "./FailureCard";

afterEach(() => {
  cleanup();
  // Restored here rather than at the end of the test that stubs it: an
  // assertion that throws must not leave a fake clipboard behind.
  vi.unstubAllGlobals();
});

function renderCard(
  message: string | undefined,
  over: { onRetry?: () => void; live?: boolean } = {},
) {
  return render(
    <MemoryRouter initialEntries={["/c/s1"]}>
      <Routes>
        <Route
          path="/c/:id"
          element={<FailureCard error={message ? { message } : undefined} {...over} />}
        />
        <Route path="/settings" element={<div>settings page</div>} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("FailureCard", () => {
  it("renders nothing when the turn did not fail", () => {
    const { container } = renderCard(undefined);
    expect(container.textContent).toBe("");
  });

  it("names the layer and the next action instead of the raw error", () => {
    const { container } = renderCard("SQLITE_FULL: database or disk is full");

    expect(screen.getByText("The disk is full")).toBeTruthy();
    expect(container.textContent).toContain("Free up space");
  });

  it("keeps the raw error reachable without the clipboard", () => {
    // The gateway serves the SPA over plain http on a LAN address, where
    // `navigator.clipboard` does not exist. A misclassified failure must still
    // leave a maintainer something to read.
    const { container } = renderCard("SQLITE_FULL: database or disk is full");

    expect(screen.getByText("Details")).toBeTruthy();
    expect(container.querySelector("details")!.textContent).toContain("SQLITE_FULL");
  });

  it("does not repeat itself when it had nothing to explain", () => {
    // An unclassified failure already shows its raw text as the message.
    const { container } = renderCard("internal: assertion failed at frame 12");

    expect(container.querySelector("details")).toBeNull();
    expect(container.textContent).toContain("internal: assertion failed at frame 12");
  });

  it("offers to try again when repeating the request could work", () => {
    const onRetry = vi.fn();
    renderCard("429 Too Many Requests", { onRetry });

    fireEvent.click(screen.getByText("Try again"));
    expect(onRetry).toHaveBeenCalledOnce();
  });

  it("withholds try-again where the same request must fail again", () => {
    // A dead key does not become live because the user pressed a button.
    renderCard("status 401 unauthorized: invalid api key", { onRetry: () => {} });

    expect(screen.queryByText("Try again")).toBeNull();
  });

  it("withholds try-again when the caller says nothing can be re-run", () => {
    // Mid-turn, and after a turn the daemon already persisted, the page drops
    // the handler. The card must not draw a button that would do nothing.
    renderCard("connection reset by peer");

    expect(screen.queryByText("Try again")).toBeNull();
  });

  it("leads a key failure to the place the key is set", () => {
    renderCard("status 401 unauthorized: invalid api key", { onRetry: () => {} });

    fireEvent.click(screen.getByText("Open settings"));
    expect(screen.getByText("settings page")).toBeTruthy();
  });

  it("keeps settings out of failures no setting can fix", () => {
    renderCard("no space left on device");

    expect(screen.queryByText("Open settings")).toBeNull();
  });

  it("copies the raw error, which is the part a maintainer needs", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText } });

    renderCard("SQLITE_FULL: database or disk is full");
    fireEvent.click(screen.getByText("Copy error"));

    expect(writeText).toHaveBeenCalledWith("SQLITE_FULL: database or disk is full");
    expect(await screen.findByText("Copied")).toBeTruthy();
  });

  it("does not hand a second failure the first one's receipt", async () => {
    // The live card never unmounts, so a sticky "Copied" would tell the user
    // they had copied a message they have not seen yet.
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText } });

    const view = render(
      <MemoryRouter>
        <FailureCard error={{ message: "no space left on device" }} />
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByText("Copy error"));
    expect(await screen.findByText("Copied")).toBeTruthy();

    view.rerender(
      <MemoryRouter>
        <FailureCard error={{ message: "429 Too Many Requests" }} />
      </MemoryRouter>,
    );
    expect(screen.getByText("Copy error")).toBeTruthy();
    expect(screen.queryByText("Copied")).toBeNull();
  });

  it("says so when there is no clipboard to copy into", () => {
    // Silence here would read as a broken button on exactly the surface where
    // the clipboard is missing.
    vi.stubGlobal("navigator", {});

    renderCard("SQLITE_FULL: database or disk is full");
    fireEvent.click(screen.getByText("Copy error"));

    expect(screen.getByText("Could not copy")).toBeTruthy();
  });

  it("announces only the failure that is happening now", () => {
    // Reopening a conversation with three old errors in it must not fire three
    // assertive announcements at a screen reader.
    const { container: settled } = renderCard("no space left on device");
    expect(settled.querySelector("[role='alert']")).toBeNull();

    const { container: live } = renderCard("no space left on device", { live: true });
    expect(live.querySelector("[role='alert']")).toBeTruthy();
  });
});

const BUDGET =
  "Monthly token budget exceeded (1013994/1000000). Increase budget.monthly_token_limit in /settings to continue.";

const setting = (key: string, value: string) => ({
  key,
  value,
  category: "default",
  label: "",
  kind: "",
  group: "",
});

function Path() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

/** The card inside the app's settings context, as the conversation page has it. */
function renderInApp(
  message: string,
  daemon = fakeDaemon({
    GetSettings: { entries: [setting("budget.monthly_token_limit", "1000000")] },
    SetSetting: {},
  }),
  onRetry?: () => void,
) {
  const view = renderApp(
    <>
      <FailureCard error={{ message }} onRetry={onRetry} live />
      <Path />
    </>,
    {
      daemon,
      route: "/c/s1",
      wrapper: (children) => <SettingsProvider>{children}</SettingsProvider>,
    },
  );
  return { ...view, daemon };
}

describe("FailureCard with a setting that fixes it", () => {
  const field = () => screen.getByLabelText("budget.monthly_token_limit") as HTMLInputElement;

  it("raises the monthly limit in place, with the suggestion prefilled", async () => {
    const { daemon } = renderInApp(BUDGET);
    await waitFor(() => expect(field().value).toBe("2,000,000"));
    expect(screen.getByText(/1,013,994 of its 1,000,000 tokens/)).toBeTruthy();
    // The fix replaces the detour, so no second way to the same place.
    expect(screen.queryByText("Open settings")).toBeNull();

    fireEvent.click(screen.getByText("Raise limit"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "budget.monthly_token_limit",
        value: "2000000",
      }),
    );
    expect(await screen.findByText("Limit raised to 2,000,000.")).toBeTruthy();
  });

  it("writes what the user typed, commas and all, on Enter", async () => {
    const { daemon } = renderInApp(BUDGET);
    await waitFor(() => expect(field().value).toBe("2,000,000"));

    fireEvent.change(field(), { target: { value: "5,000,000" } });
    fireEvent.keyDown(field(), { key: "Enter" });
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "budget.monthly_token_limit",
        value: "5000000",
      }),
    );
  });

  it("hands back the retry once the limit is raised, and not before", async () => {
    const onRetry = vi.fn();
    renderInApp(BUDGET, undefined, onRetry);
    await waitFor(() => expect(field()).toBeTruthy());
    // Retrying before the raise is certain to fail again.
    expect(screen.queryByText("Try again")).toBeNull();

    fireEvent.click(screen.getByText("Raise limit"));
    fireEvent.click(await screen.findByText("Try again"));
    expect(onRetry).toHaveBeenCalledOnce();
  });

  it("tells a settled card's reader to send again, since it has no retry", async () => {
    renderInApp(BUDGET);
    await waitFor(() => expect(field()).toBeTruthy());
    fireEvent.click(screen.getByText("Raise limit"));
    expect(await screen.findByText("Send your message again.")).toBeTruthy();
  });

  it("shows the daemon's refusal and keeps the field", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: [setting("budget.monthly_token_limit", "1000000")] },
      SetSetting: new BorgError(INVALID_ARGUMENT, "value must be a whole number"),
    });
    renderInApp(BUDGET, daemon);
    await waitFor(() => expect(field()).toBeTruthy());

    fireEvent.change(field(), { target: { value: "lots" } });
    fireEvent.click(screen.getByText("Raise limit"));
    expect(await screen.findByText(/value must be a whole number/)).toBeTruthy();
    expect(field()).toBeTruthy();
  });

  it("opens the named setting directly when the error names one", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: [setting("llm.base_url", "")] },
    });
    renderInApp(
      "Provider `custom` needs `llm.base_url`. Set it via `borg settings set llm.base_url <url>`.",
      daemon,
    );
    fireEvent.click(await screen.findByText("Open this setting"));
    expect(screen.getByTestId("path").textContent).toBe("/settings/key/llm.base_url");
  });

  it("falls back to Settings for a key this daemon does not have", async () => {
    // An old error can name a key a newer daemon renamed; a dead link is worse
    // than the settings list.
    const daemon = fakeDaemon({ GetSettings: { entries: [] } });
    renderInApp("status 401 unauthorized. Fix with borg settings set llm.legacy_key", daemon);
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "GetSettings")).toBe(true));
    fireEvent.click(screen.getByText("Open settings"));
    expect(screen.getByTestId("path").textContent).toBe("/settings");
  });

  it("degrades to the plain link outside the settings context", () => {
    // Transcript cells render in isolation in tests and before the provider mounts.
    renderCard(BUDGET);
    expect(screen.queryByText("Raise limit")).toBeNull();
    fireEvent.click(screen.getByText("Open settings"));
    expect(screen.getByText("settings page")).toBeTruthy();
  });
});
