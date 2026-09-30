import { describe, expect, it } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { ThemeProvider } from "@borg/ui";
import { installMatchMedia } from "@borg/ui/test/matchMedia";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { setupStatus } from "../../test/setupStatus";
import { SettingsProvider } from "../../lib/settings";
import { SettingsPage } from "./SettingsPage";
import { advancedGroupOf, findBasicSetting } from "./catalog";

const media = installMatchMedia(() => false);
const mobile = media.controller("(max-width: 700px)");

const entry = (key: string, value: string, category = "default") => ({
  key,
  value,
  category,
  label: "",
  kind: "",
  group: "",
});

const REGISTRY = [
  entry("provider", "anthropic"),
  entry("model", "claude-opus-5"),
  entry("llm.thinking", "medium"),
  entry("conversation.collaboration_mode", "execute"),
  entry("heartbeat.enabled", "true", "db"),
  entry("agents.max_depth", "3"),
  entry("standalone", "yes"),
];

const DAEMON = { GetSettings: { entries: REGISTRY }, GetSetupStatus: setupStatus() };

function mount(daemon = fakeDaemon(DAEMON)) {
  return renderApp(<SettingsPage />, {
    daemon,
    wrapper: (children) => (
      <ThemeProvider>
        <SettingsProvider>{children}</SettingsProvider>
      </ThemeProvider>
    ),
  });
}

describe("advancedGroupOf", () => {
  it("buckets a dotted key under its section", () => {
    expect(advancedGroupOf("llm.thinking")).toBe("Llm");
    expect(advancedGroupOf("heartbeat.min_interval_secs")).toBe("Heartbeat");
  });

  it("puts an undotted key in General rather than inventing a group", () => {
    expect(advancedGroupOf("provider")).toBe("General");
  });
});

describe("findBasicSetting", () => {
  it("finds a curated setting by its route segment", () => {
    expect(findBasicSetting("thinking")?.key).toBe("llm.thinking");
  });

  it("returns nothing for a segment that names no setting", () => {
    expect(findBasicSetting("nonsense")).toBeUndefined();
  });
});

describe("SettingsPage", () => {
  it("shows each curated setting's current value, named as the catalog names it", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    // Provider and model ids read as their display names once the catalog
    // answers; "anthropic" and "claude-opus-5" are what the daemon stores.
    await waitFor(() => expect(screen.getByText("Anthropic")).toBeTruthy());
    expect(screen.getByText("Opus 5")).toBeTruthy();
    expect(screen.queryByText("claude-opus-5")).toBeNull();
    // Stored option values read as words, not identifiers.
    expect(screen.getByText("Medium")).toBeTruthy();
    expect(screen.getByText("Execute")).toBeTruthy();
    expect(screen.queryByText("medium")).toBeNull();
  });

  it("names the provider default when no model is set", async () => {
    // The daemon adopts the catalog default for an empty `model`; a blank row
    // would say Borg has no model when it is answering with one.
    const daemon = fakeDaemon({
      ...DAEMON,
      GetSettings: { entries: [entry("provider", "anthropic"), entry("model", "")] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Opus 5")).toBeTruthy());
  });

  it("shows a value the options do not list exactly as stored", async () => {
    const daemon = fakeDaemon({
      ...DAEMON,
      GetSettings: { entries: [entry("llm.thinking", "xhigh")] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("xhigh")).toBeTruthy());
  });

  it("makes every row a button, so the keyboard and screen readers reach it", async () => {
    mount();
    await waitFor(() => expect(screen.getByRole("button", { name: /Provider/ })).toBeTruthy());
    for (const name of [/^Model/, /^Thinking level/, /^Mode(?!l)/, /^Appearance/]) {
      expect(screen.getByRole("button", { name })).toBeTruthy();
    }
    const fold = screen.getByRole("button", { name: /Advanced/ });
    expect(fold.getAttribute("aria-expanded")).toBe("false");
    act(() => fold.click());
    expect(fold.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByRole("button", { name: /heartbeat\.enabled/ })).toBeTruthy();
  });

  it("falls back to the raw id when the catalog cannot answer", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: REGISTRY },
      GetSetupStatus: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("anthropic")).toBeTruthy());
    expect(screen.getByText("claude-opus-5")).toBeTruthy();
  });

  it("shows a dash for a curated setting the daemon does not report", async () => {
    // Better an honest gap than a value invented by the page.
    const daemon = fakeDaemon({
      ...DAEMON,
      GetSettings: { entries: [entry("provider", "anthropic")] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Anthropic")).toBeTruthy());
    expect(screen.getAllByText("—").length).toBeGreaterThan(0);
  });

  it("keeps the whole registry behind the Advanced fold", async () => {
    // Progressive disclosure: the default view is the smallest set that
    // finishes the job.
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    expect(screen.queryByText("heartbeat.enabled")).toBeNull();

    act(() => screen.getByText("Advanced").click());
    expect(screen.getByText("heartbeat.enabled")).toBeTruthy();
  });

  it("does not repeat a curated key inside Advanced", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    expect(screen.queryByText("llm.thinking")).toBeNull();
    expect(screen.getByText("agents.max_depth")).toBeTruthy();
  });

  it("groups advanced keys by section, alphabetically", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    const groups = ["Agents", "General", "Heartbeat"].map((name) => screen.getByText(name));
    expect(groups.map((g) => g.textContent)).toEqual(["Agents", "General", "Heartbeat"]);
  });

  it("closes the fold again", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    act(() => screen.getByText("Advanced").click());
    expect(screen.queryByText("heartbeat.enabled")).toBeNull();
  });

  it("reports a failed registry load instead of an empty settings list", async () => {
    const daemon = fakeDaemon({
      GetSettings: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Could not load settings.")).toBeTruthy());
  });

  it("lists the pages the mobile tab bar cannot hold", async () => {
    // Five tabs is the ceiling; without this the remaining pages are
    // unreachable on a phone.
    mobile.set(true);
    try {
      mount();
      await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
      expect(screen.getByText("Scheduled")).toBeTruthy();
      expect(screen.getByText("Usage")).toBeTruthy();
    } finally {
      mobile.set(false);
    }
  });

  it("shows no such list on desktop, where the sidebar already has them", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    expect(screen.queryByText("Scheduled")).toBeNull();
    expect(screen.queryByText("Usage")).toBeNull();
  });
});
