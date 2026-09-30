import { describe, expect, it, vi } from "vitest";
import type { DaemonSetting } from "../../lib/settings";
import { buildCommands, type CommandContext, type RouteRow } from "./commands";
import { rank } from "./score";

const ROUTES: RouteRow[] = [{ id: "settings", path: "/settings", label: "Settings" }];

function setting(over: Partial<DaemonSetting> & { key: string }): DaemonSetting {
  return {
    value: "",
    overridden: false,
    label: "",
    kind: "",
    group: "",
    ...over,
  };
}

function context(over: Partial<CommandContext> = {}): CommandContext {
  return {
    navigate: vi.fn(),
    dispatch: vi.fn(),
    hintFor: () => undefined,
    settings: new Map(),
    saveSetting: vi.fn(),
    sessions: [],
    skills: [],
    skillRecency: [],
    noteSkillUse: vi.fn(),
    themePreference: "system",
    setThemePreference: vi.fn(),
    searching: false,
    ...over,
  };
}

function settingsMap(...entries: DaemonSetting[]): Map<string, DaemonSetting> {
  return new Map(entries.map((e) => [e.key, e]));
}

describe("buildCommands", () => {
  it("names a curated setting by its label, never by its key", () => {
    const ctx = context({
      settings: settingsMap(
        setting({
          key: "conversation.show_thinking",
          value: "true",
          kind: "bool",
          label: "Show reasoning",
          group: "Conversation",
        }),
      ),
    });

    const labels = buildCommands(ctx, ROUTES).map((c) => c.label);
    expect(labels).toContain("Show reasoning");
    expect(labels).not.toContain("conversation.show_thinking");
  });

  it("gives a boolean setting one row that flips it, reporting the live state", () => {
    const saveSetting = vi.fn();
    const ctx = context({
      saveSetting,
      settings: settingsMap(
        setting({
          key: "evolution.enabled",
          value: "true",
          kind: "bool",
          label: "Evolution",
          group: "Personality",
        }),
      ),
    });

    const rows = buildCommands(ctx, ROUTES).filter((c) => c.id === "setting:evolution.enabled");
    expect(rows).toHaveLength(1);
    expect(rows[0]!.detail).toBe("on");

    rows[0]!.run();
    expect(saveSetting).toHaveBeenCalledWith("evolution.enabled", "false");
  });

  it("reports off, and turns on, a boolean that is currently false", () => {
    const saveSetting = vi.fn();
    const ctx = context({
      saveSetting,
      settings: settingsMap(
        setting({
          key: "tts.enabled",
          value: "false",
          kind: "bool",
          label: "TTS provider enabled",
        }),
      ),
    });

    const row = buildCommands(ctx, ROUTES).find((c) => c.id === "setting:tts.enabled");
    expect(row?.detail).toBe("off");
    row?.run();
    expect(saveSetting).toHaveBeenCalledWith("tts.enabled", "true");
  });

  it("keeps a toggle's palette open, because the state change is the feedback", () => {
    const ctx = context({
      settings: settingsMap(
        setting({ key: "skills.enabled", value: "true", kind: "bool", label: "Allow skills" }),
      ),
    });
    const row = buildCommands(ctx, ROUTES).find((c) => c.id === "setting:skills.enabled");
    expect(row?.keepOpen).toBe(true);
  });

  it("navigates rather than toggles when a curated setting is not boolean", () => {
    const navigate = vi.fn();
    const saveSetting = vi.fn();
    const ctx = context({
      navigate,
      saveSetting,
      settings: settingsMap(
        setting({ key: "llm.thinking", value: "medium", kind: "select", label: "Thinking level" }),
      ),
    });

    const row = buildCommands(ctx, ROUTES).find((c) => c.id === "setting:llm.thinking");
    expect(row?.detail).toBeUndefined();
    row?.run();
    expect(saveSetting).not.toHaveBeenCalled();
    expect(navigate).toHaveBeenCalledWith("/settings/key/llm.thinking");
  });

  it("does not treat a bool-kinded setting with a non-boolean value as a toggle", () => {
    // A key whose stored value drifted must not offer a flip that would write
    // a nonsense value over it.
    const ctx = context({
      settings: settingsMap(
        setting({ key: "weird.flag", value: "maybe", kind: "bool", label: "Weird flag" }),
      ),
    });
    const row = buildCommands(ctx, ROUTES).find((c) => c.id === "setting:weird.flag");
    expect(row?.detail).toBeUndefined();
  });

  it("hides uncurated keys until the user types", () => {
    const uncurated = settingsMap(setting({ key: "heartbeat.min_interval_secs", value: "60" }));

    const resting = buildCommands(context({ settings: uncurated }), ROUTES);
    expect(resting.some((c) => c.label === "heartbeat.min_interval_secs")).toBe(false);

    const searching = buildCommands(context({ settings: uncurated, searching: true }), ROUTES);
    expect(searching.some((c) => c.label === "heartbeat.min_interval_secs")).toBe(true);
  });

  it("keeps a named setting above the raw keys of its own subsystem", () => {
    // Through the real ranker, because array order alone proves nothing: a raw
    // key matches its own letters exactly, while the curated row only matches
    // through keywords and pays a penalty for it. Relevance alone put
    // `tts.timeout_ms` above "TTS provider enabled".
    const ctx = context({
      searching: true,
      settings: settingsMap(
        setting({ key: "tts.models", value: "[]" }),
        setting({ key: "tts.timeout_ms", value: "30" }),
        setting({
          key: "tts.enabled",
          value: "true",
          kind: "bool",
          label: "TTS provider enabled",
        }),
      ),
    });

    const ranked = rank(buildCommands(ctx, ROUTES), "tts").map((c) => c.label);
    expect(ranked[0]).toBe("TTS provider enabled");
  });

  it("checks the appearance row that is actually applied", () => {
    const commands = buildCommands(context({ themePreference: "dark" }), ROUTES);
    const appearance = commands.filter((c) => c.id.startsWith("theme:"));
    expect(appearance.filter((c) => c.active).map((c) => c.id)).toEqual(["theme:dark"]);
  });

  it("applies the appearance a row names", () => {
    const setThemePreference = vi.fn();
    const commands = buildCommands(context({ setThemePreference }), ROUTES);
    commands.find((c) => c.id === "theme:light")?.run();
    expect(setThemePreference).toHaveBeenCalledWith("light");
  });

  it("survives a settings snapshot that has not loaded yet", () => {
    const commands = buildCommands(context({ settings: undefined, searching: true }), ROUTES);
    expect(commands.some((c) => c.id === "action:new")).toBe(true);
  });

  describe("skill rows", () => {
    const SKILLS = [
      { name: "git", description: "Work with repositories" },
      { name: "weather", description: "Look up a forecast" },
    ];

    it("writes prompt text rather than acting, because the agent picks the skill", () => {
      const commands = buildCommands(context({ skills: SKILLS }), ROUTES);
      const git = commands.find((c) => c.id === "skill:git");
      expect(git?.insert).toBe("Use the git skill to ");
      expect(git?.label).toBe("git");
    });

    it("offers what the skill says it is for, so the name does not have to carry it", () => {
      const commands = buildCommands(context({ skills: SKILLS }), ROUTES);
      expect(commands.find((c) => c.id === "skill:weather")?.detail).toBe("Look up a forecast");
      // Searchable by purpose: nobody types "weather" when they want a forecast.
      expect(rank(commands, "forecast").map((c) => c.id)).toContain("skill:weather");
    });

    it("leads with the skill last used", () => {
      const commands = buildCommands(
        context({ skills: SKILLS, skillRecency: ["weather"] }),
        ROUTES,
      );
      const skills = commands.filter((c) => c.id.startsWith("skill:"));
      expect(skills.map((c) => c.id)).toEqual(["skill:weather", "skill:git"]);
    });

    it("records the use, so the next list leads with it", () => {
      const noteSkillUse = vi.fn();
      const commands = buildCommands(context({ skills: SKILLS, noteSkillUse }), ROUTES);
      commands.find((c) => c.id === "skill:git")?.run();
      expect(noteSkillUse).toHaveBeenCalledWith("git");
    });

    it("has no skill rows on a surface with nowhere to write them", () => {
      // ⌘K passes no skills: prompt text needs a prompt box.
      const commands = buildCommands(context({ skills: [] }), ROUTES);
      expect(commands.some((c) => c.id.startsWith("skill:"))).toBe(false);
    });
  });
});

describe("marking every conversation read", () => {
  it("is not offered when no dot is lit", () => {
    // A row that would do nothing is a row that teaches nothing.
    const ids = buildCommands(context(), ROUTES).map((c) => c.id);

    expect(ids).not.toContain("action:mark-read");
  });

  it("clears the dots when something is unread", () => {
    const markAllRead = vi.fn();

    const row = buildCommands(context({ markAllRead }), ROUTES).find(
      (c) => c.id === "action:mark-read",
    );
    row?.run();

    expect(row?.label).toBe("Mark all conversations read");
    expect(markAllRead).toHaveBeenCalledTimes(1);
  });
});

describe("starting over", () => {
  const cases: [string, Partial<CommandContext>][] = [
    ["nothing is unread", {}],
    ["mark-all-read is on offer too", { markAllRead: () => {} }],
  ];

  it.each(cases)("answers `clear` with a new conversation when %s", (_name, over) => {
    const commands = buildCommands(context({ searching: true, ...over }), ROUTES);

    expect(rank(commands, "clear")[0]?.id).toBe("action:new");
    expect(rank(commands, "reset")[0]?.id).toBe("action:new");
  });

  it("still finds mark-all-read by the words that name it", () => {
    const commands = buildCommands(context({ searching: true, markAllRead: () => {} }), ROUTES);

    expect(rank(commands, "unread")[0]?.id).toBe("action:mark-read");
    expect(rank(commands, "clear").map((c) => c.id)).toContain("action:mark-read");
  });
});
