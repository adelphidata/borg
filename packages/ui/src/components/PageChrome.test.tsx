import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { installMatchMedia } from "../test/matchMedia";
import { MobileTitle } from "./MobileTitle";
import { PageChrome } from "./PageChrome";

const QUERY = "(max-width: 700px)";
const media = installMatchMedia(() => false);
const breakpoint = media.controller(QUERY);

describe("PageChrome", () => {
  it("shows the desktop header with title, meta and actions", () => {
    breakpoint.set(false);
    render(
      <PageChrome title="Usage" meta="last 7 days" headerActions={<button>Refresh</button>}>
        <p>body</p>
      </PageChrome>,
    );
    expect(screen.getByText("Usage")).toBeTruthy();
    expect(screen.getByText("last 7 days")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Refresh" })).toBeTruthy();
    expect(screen.getByText("body")).toBeTruthy();
  });

  it("wraps the meta in its own element, and renders none without meta", () => {
    breakpoint.set(false);
    const { container, unmount } = render(
      <PageChrome title="Usage" meta="last 7 days">
        <p>body</p>
      </PageChrome>,
    );
    // The meta needs its own styled span — not a bare text node in the header.
    expect(screen.getByText("last 7 days").tagName).toBe("SPAN");
    expect(container.querySelector(".text-ink3")?.textContent).toBe("last 7 days");
    unmount();

    const bare = render(
      <PageChrome title="Usage">
        <p>body</p>
      </PageChrome>,
    );
    // No meta, no slot: an empty styled span would still take header space.
    expect(bare.container.querySelector(".text-ink3")).toBeNull();
  });

  it("lets a double-click or Enter on the title rename it, and Escape back out", async () => {
    breakpoint.set(false);
    const onRename = vi.fn().mockResolvedValue(true);
    render(
      <PageChrome title="Ship it" onRename={onRename}>
        <p>body</p>
      </PageChrome>,
    );
    const title = screen.getByRole("heading", { level: 1, name: "Ship it" });
    expect(title.getAttribute("title")).toBe("Double-click to rename");
    fireEvent.doubleClick(title);
    const field = screen.getByRole("textbox", { name: "Conversation name" }) as HTMLInputElement;
    expect(field.value).toBe("Ship it");
    fireEvent.change(field, { target: { value: "Ship it today" } });
    fireEvent.keyDown(field, { key: "Enter" });
    expect(onRename).toHaveBeenCalledWith("Ship it today");
    await act(async () => {});
    expect(screen.queryByRole("textbox")).toBeNull();

    // The keyboard reaches it too.
    fireEvent.keyDown(screen.getByRole("heading", { level: 1 }), { key: "Enter" });
    const again = screen.getByRole("textbox", { name: "Conversation name" });
    fireEvent.change(again, { target: { value: "Dropped" } });
    fireEvent.keyDown(again, { key: "Escape" });
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(onRename).toHaveBeenCalledTimes(1);
  });

  it("does not rename to the same or an empty name, and stays open on a refusal", async () => {
    breakpoint.set(false);
    const onRename = vi.fn().mockResolvedValue(false);
    render(
      <PageChrome title="Ship it" onRename={onRename}>
        <p>body</p>
      </PageChrome>,
    );
    fireEvent.doubleClick(screen.getByRole("heading", { level: 1 }));
    const field = screen.getByRole("textbox", { name: "Conversation name" });
    fireEvent.change(field, { target: { value: "   " } });
    fireEvent.blur(field);
    expect(onRename).not.toHaveBeenCalled();
    expect(screen.queryByRole("textbox")).toBeNull();

    fireEvent.doubleClick(screen.getByRole("heading", { level: 1 }));
    const same = screen.getByRole("textbox", { name: "Conversation name" });
    fireEvent.keyDown(same, { key: "Enter" });
    expect(onRename).not.toHaveBeenCalled();
    expect(screen.queryByRole("textbox")).toBeNull();

    fireEvent.doubleClick(screen.getByRole("heading", { level: 1 }));
    const refused = screen.getByRole("textbox", { name: "Conversation name" });
    fireEvent.change(refused, { target: { value: "New" } });
    fireEvent.keyDown(refused, { key: "Enter" });
    await act(async () => {});
    expect(onRename).toHaveBeenCalledWith("New");
    expect(screen.getByRole("textbox", { name: "Conversation name" })).toBeTruthy();
  });

  it("shows a plain heading when nothing can rename it", () => {
    breakpoint.set(false);
    render(
      <PageChrome title="Usage">
        <p>body</p>
      </PageChrome>,
    );
    const title = screen.getByRole("heading", { level: 1, name: "Usage" });
    fireEvent.doubleClick(title);
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(title.getAttribute("tabindex")).toBeNull();
  });

  it("drops the desktop header at phone width but keeps the page's heading", () => {
    // Pages show their title through MobileTitle; the h1 stays for assistive
    // tech only, so every screen still has exactly one.
    breakpoint.set(true);
    render(
      <PageChrome title="Usage" meta="last 7 days">
        <p>body</p>
      </PageChrome>,
    );
    expect(screen.queryByRole("banner")).toBeNull();
    expect(screen.getByRole("heading", { level: 1, name: "Usage" }).className).toContain("sr-only");
    expect(screen.queryByText("last 7 days")).toBeNull();
    expect(screen.getByText("body")).toBeTruthy();
  });

  it("names the page with an h1 in the desktop header", () => {
    breakpoint.set(false);
    render(
      <PageChrome title="Usage">
        <p>body</p>
      </PageChrome>,
    );
    expect(screen.getByRole("heading", { level: 1, name: "Usage" }).className).not.toContain(
      "sr-only",
    );
  });

  it("shows the brand header on mobile only where the page asked for it", () => {
    breakpoint.set(true);
    const { unmount } = render(
      <PageChrome title="Conversation" mobileBrandHeader>
        <p>body</p>
      </PageChrome>,
    );
    expect(screen.getByText("BORG")).toBeTruthy();
    unmount();

    render(
      <PageChrome title="Usage">
        <p>body</p>
      </PageChrome>,
    );
    expect(screen.queryByText("BORG")).toBeNull();
  });

  it("renders the companion panel on desktop", () => {
    breakpoint.set(false);
    render(
      <PageChrome title="Conversation" aside={<aside>agents</aside>}>
        <p>body</p>
      </PageChrome>,
    );
    expect(screen.getByText("agents")).toBeTruthy();
  });

  it("drops the companion panel at phone width, without the caller knowing", () => {
    // There is no room for two columns, and the page should not have to ask.
    breakpoint.set(true);
    render(
      <PageChrome title="Conversation" aside={<aside>agents</aside>}>
        <p>body</p>
      </PageChrome>,
    );
    expect(screen.queryByText("agents")).toBeNull();
    expect(screen.getByText("body")).toBeTruthy();
  });
});

describe("MobileTitle", () => {
  it("renders only at the mobile breakpoint", () => {
    breakpoint.set(false);
    const { unmount } = render(<MobileTitle>Usage</MobileTitle>);
    expect(screen.queryByText("Usage")).toBeNull();
    unmount();

    breakpoint.set(true);
    render(<MobileTitle>Usage</MobileTitle>);
    expect(screen.getByText("Usage")).toBeTruthy();
  });

  it("appends the caller's className to the heading styles, nothing else", () => {
    breakpoint.set(true);
    render(<MobileTitle>Plain</MobileTitle>);
    render(<MobileTitle className="pt-4">Styled</MobileTitle>);
    const base = screen.getByText("Plain").className;
    expect(screen.getByText("Styled").className).toBe(`${base} pt-4`);
  });
});
