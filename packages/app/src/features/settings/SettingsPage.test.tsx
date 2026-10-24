import { beforeEach, describe, expect, it } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { useLocation } from "react-router-dom";
import { ThemeProvider } from "@borg/ui";
import { installMatchMedia } from "@borg/ui/test/matchMedia";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { setupStatus } from "../../test/setupStatus";
import { SettingsProvider } from "../../lib/settings";
import { SettingsPage } from "./SettingsPage";
import { groupAdvanced } from "./AdvancedSettings";
import { advancedCategoryOf, findBasicSetting } from "./catalog";

const media = installMatchMedia(() => false);

// Open folds are kept for the tab; each test starts from a closed page.
beforeEach(() => sessionStorage.clear());
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
  entry("heartbeat.last_fired_at", "1761264000", "db"),
  entry("agents.max_depth", "3"),
  entry("standalone", "yes"),
];

const DAEMON = { GetSettings: { entries: REGISTRY }, GetSetupStatus: setupStatus() };

function Path() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(daemon = fakeDaemon(DAEMON)) {
  return renderApp(
    <>
      <SettingsPage />
      <Path />
    </>,
    {
      daemon,
      wrapper: (children) => (
        <ThemeProvider>
          <SettingsProvider>{children}</SettingsProvider>
        </ThemeProvider>
      ),
    },
  );
}

describe("advancedCategoryOf", () => {
  it("files a key by what it is for, not by its prefix", () => {
    expect(advancedCategoryOf("llm.thinking").name).toBe("Model");
    expect(advancedCategoryOf("tts.default_voice").name).toBe("Voice & Media");
    expect(advancedCategoryOf("audio.language").name).toBe("Voice & Media");
    expect(advancedCategoryOf("heartbeat.min_interval_secs").name).toBe("Heartbeat");
    expect(advancedCategoryOf("temperature").name).toBe("Model");
  });

  it("puts a prefix nobody listed in Other rather than dropping it", () => {
    expect(advancedCategoryOf("brand_new.knob").name).toBe("Other");
    expect(advancedCategoryOf("standalone").name).toBe("Other");
  });
});

describe("groupAdvanced", () => {
  const row = (key: string, overridden = false) => ({
    key,
    value: "",
    overridden,
    label: "",
    kind: "",
    group: "",
  });

  it("orders categories as listed and runs inside them by prefix, then name", () => {
    const groups = groupAdvanced(
      [
        row("tts.enabled"),
        row("voice.enabled", true),
        row("voice.auto_tts"),
        row("llm.max_retries", true),
        row("temperature"),
        row("provider"),
      ],
      new Set(["provider"]),
    );
    expect(groups.map((g) => g.category.name)).toEqual(["Model", "Voice & Media"]);
    const [model, voice] = groups;
    expect(model!.sections.map((s) => s.prefix)).toEqual(["", "llm"]);
    expect(voice!.sections.map((s) => [s.prefix, s.rows.map((r) => r.key)])).toEqual([
      ["voice", ["voice.auto_tts", "voice.enabled"]],
      ["tts", ["tts.enabled"]],
    ]);
    expect([voice!.total, voice!.changed]).toEqual([3, 1]);
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
    const category = screen.getByRole("button", { name: /^Heartbeat/ });
    expect(category.getAttribute("aria-expanded")).toBe("false");
    act(() => category.click());
    expect(category.getAttribute("aria-expanded")).toBe("true");
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
    expect(screen.queryByText("Heartbeat")).toBeNull();

    act(() => screen.getByText("Advanced").click());
    // Opening Advanced shows categories, not keys; a category shows its keys.
    expect(screen.queryByTitle("heartbeat.enabled")).toBeNull();
    act(() => screen.getByText("Heartbeat").click());
    expect(screen.getByTitle("heartbeat.enabled")).toBeTruthy();
  });

  it("leaves the heartbeat's own fire stamp out of Advanced", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    act(() => screen.getByText("Heartbeat").click());
    expect(screen.getByTitle("heartbeat.enabled")).toBeTruthy();
    expect(screen.queryByTitle("heartbeat.last_fired_at")).toBeNull();
  });

  it("does not repeat a curated key inside Advanced", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    // Thinking level is curated, so Model has nothing left to show. Folds are
    // the only buttons with aria-expanded; the curated Model row has none.
    const folds = screen.getAllByRole("button", { expanded: false });
    expect(folds.some((b) => b.textContent?.startsWith("Agents & Goals"))).toBe(true);
    expect(folds.some((b) => b.textContent?.startsWith("Model"))).toBe(false);
    act(() => screen.getByText("Agents & Goals").click());
    expect(screen.getByTitle("agents.max_depth")).toBeTruthy();
    expect(screen.queryByTitle("llm.thinking")).toBeNull();
  });

  it("lists categories in screen order with counts and what changed", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    const folds = screen
      .getAllByRole("button", { expanded: false })
      .map((b) => b.textContent ?? "")
      .filter((t) => /Agents & Goals|Heartbeat|Other/.test(t));
    expect(folds.map((t) => t.match(/Agents & Goals|Heartbeat|Other/)?.[0])).toEqual([
      "Agents & Goals",
      "Heartbeat",
      "Other",
    ]);
    expect(screen.getByText("1 changed")).toBeTruthy();
  });

  it("opens the guided voice page from its category, above the raw keys", async () => {
    mount(
      fakeDaemon({
        ...DAEMON,
        GetSettings: { entries: [...REGISTRY, entry("tts.enabled", "false")] },
      }),
    );
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    act(() => screen.getByText("Voice & Media").click());
    expect(screen.getByTitle("tts.enabled")).toBeTruthy();
    expect(advancedCategoryOf("telephony.provider").name).toBe("Voice & Media");
    act(() => screen.getByRole("button", { name: /Voice & phone/ }).click());
    expect(screen.getByTestId("path").textContent).toBe("/settings/voice");
  });

  it("shows counts with grouped digits and lists by size, not as raw JSON", async () => {
    mount(
      fakeDaemon({
        ...DAEMON,
        GetSettings: {
          entries: [
            ...REGISTRY,
            entry("agents.child_timeout_ms", "120000"),
            entry("agents.allowed_roles", '["worker","reviewer"]'),
          ],
        },
      }),
    );
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    act(() => screen.getByText("Agents & Goals").click());
    expect(screen.getByTitle("agents.child_timeout_ms").textContent).toContain("120,000");
    expect(screen.getByTitle("agents.allowed_roles").textContent).toContain("2 items");
    expect(screen.getByTitle("agents.allowed_roles").textContent).not.toContain("worker");
  });

  it("remembers open folds for the tab across a remount", async () => {
    const first = mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    act(() => screen.getByText("Heartbeat").click());
    first.unmount();
    mount();
    await waitFor(() => expect(screen.getByTitle("heartbeat.enabled")).toBeTruthy());
  });

  it("closes the fold again", async () => {
    mount();
    await waitFor(() => expect(screen.getByText("Provider")).toBeTruthy());
    act(() => screen.getByText("Advanced").click());
    act(() => screen.getByText("Heartbeat").click());
    act(() => screen.getByText("Heartbeat").click());
    expect(screen.queryByTitle("heartbeat.enabled")).toBeNull();
    act(() => screen.getByText("Advanced").click());
    expect(screen.queryByText("Heartbeat")).toBeNull();
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
