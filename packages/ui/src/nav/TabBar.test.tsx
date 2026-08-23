import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { TabBar } from "./TabBar";
import { LogoIcon } from "../components/icons";
import type { AppRoute } from "../types";

const page = () => null;

const ROUTES: AppRoute[] = [
  {
    id: "conversation",
    path: "/",
    label: "Conversation",
    tabLabel: "Talk",
    icon: LogoIcon,
    page,
    section: "primary",
  },
  {
    id: "sessions",
    path: "/sessions",
    label: "Conversations",
    tabLabel: "History",
    icon: LogoIcon,
    page,
    section: "primary",
  },
  {
    id: "setting-detail",
    path: "/settings/:id",
    label: "Setting",
    tabLabel: "Setting",
    icon: LogoIcon,
    page,
    section: "secondary",
    nav: { tabBar: false },
  },
];

function renderAt(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <TabBar routes={ROUTES} />
    </MemoryRouter>,
  );
}

describe("TabBar", () => {
  it("shows a tab per route, minus the ones that opted out", () => {
    // A detail route is reachable but is not a destination of its own.
    renderAt("/");
    expect(screen.getAllByRole("link").map((l) => l.textContent)).toEqual(["Talk", "History"]);
  });

  it("marks the current tab as active", () => {
    renderAt("/sessions");
    expect(screen.getByRole("link", { name: "History" }).getAttribute("aria-current")).toBe("page");
    expect(screen.getByRole("link", { name: "Talk" }).getAttribute("aria-current")).toBeNull();
  });

  it("keeps the root tab inactive on a deeper route", () => {
    // `end` on "/" is what stops the root matching every path as a prefix.
    renderAt("/sessions");
    expect(screen.getByRole("link", { name: "Talk" }).getAttribute("aria-current")).toBeNull();
  });

  it("links each tab at its own path", () => {
    renderAt("/");
    expect(screen.getByRole("link", { name: "History" }).getAttribute("href")).toBe("/sessions");
  });

  it("keeps a parent tab lit on its own detail pages", () => {
    // Only the root is exact-matched; every other tab prefix-matches so a
    // drill-down does not un-light the tab the user is inside.
    renderAt("/sessions/abc123");
    expect(screen.getByRole("link", { name: "History" }).getAttribute("aria-current")).toBe("page");
    expect(screen.getByRole("link", { name: "Talk" }).getAttribute("aria-current")).toBeNull();
  });

  it("distinguishes the active tab's dot and label from the inactive ones", () => {
    renderAt("/sessions");
    const [dot, label] = Array.from(
      screen.getByRole("link", { name: "History" }).querySelectorAll("span"),
    );
    const [idleDot, idleLabel] = Array.from(
      screen.getByRole("link", { name: "Talk" }).querySelectorAll("span"),
    );

    // Tokens, not full class strings: the contract is the lit-dot/dim-label
    // difference between active and inactive, not the exact styling.
    expect(dot.classList.contains("bg-neon")).toBe(true);
    expect(dot.classList.contains("bg-transparent")).toBe(false);
    expect(idleDot.classList.contains("bg-transparent")).toBe(true);
    expect(idleDot.classList.contains("bg-neon")).toBe(false);

    expect(label.classList.contains("text-ink")).toBe(true);
    expect(idleLabel.classList.contains("text-ink3")).toBe(true);
    expect(idleLabel.classList.contains("text-ink")).toBe(false);
  });
});
