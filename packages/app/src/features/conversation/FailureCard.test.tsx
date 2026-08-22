import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
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
