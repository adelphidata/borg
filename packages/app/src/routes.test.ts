import { describe, expect, it } from "vitest";
import { routes, visibleRoutes } from "./routes";

/** What the sidebar's primary section lists, in order. */
const sidebarPrimary = (meetings: boolean) =>
  visibleRoutes(routes, { meetings })
    .filter((r) => r.section === "primary" && r.nav?.sidebar !== false)
    .map((r) => r.label);

describe("visibleRoutes", () => {
  it("keeps Meetings out of the navigation while the feature is off", () => {
    expect(visibleRoutes(routes, { meetings: false }).some((r) => r.id === "meetings")).toBe(false);
    expect(sidebarPrimary(false)).toEqual(["Chats", "Connections", "Scheduled"]);
  });

  it("puts Meetings directly under Chats when it is on", () => {
    expect(sidebarPrimary(true)).toEqual(["Chats", "Meetings", "Connections", "Scheduled"]);
  });

  it("keeps the phone's tab bar at five with Meetings on, Meetings reached from More", () => {
    const tabs = visibleRoutes(routes, { meetings: true }).filter((r) => r.nav?.tabBar !== false);
    expect(tabs.map((r) => r.id)).not.toContain("meetings");
    expect(tabs.length).toBeLessThanOrEqual(5);
  });
});
