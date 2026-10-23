import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { STORAGE_KEYS, ThemeProvider } from "@borg/ui";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { SETUP_CATALOG, setupStatus } from "../../test/setupStatus";
import { SettingsProvider } from "../../lib/settings";
import { SettingDetailPage } from "./SettingDetailPage";

const entry = (key: string, value: string, category = "default") => ({
  key,
  value,
  category,
  label: "",
  kind: "",
  group: "",
});

const REGISTRY = [
  entry("user.agent_name", ""),
  entry("user.name", ""),
  entry("llm.thinking", "medium"),
  entry("provider", "anthropic"),
  entry("model", "claude-opus-5"),
  entry("agents.max_depth", "3", "db"),
  entry("gateway.port", "7842"),
];

const DAEMON = {
  GetSettings: { entries: REGISTRY },
  GetSetupStatus: setupStatus(),
  ListCredentials: { providers: [] },
  ListApiKeys: { entries: [] },
};

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(route: string, daemon = fakeDaemon(DAEMON)) {
  return renderApp(
    <Routes>
      <Route path="/settings/:id" element={<SettingDetailPage />} />
      <Route path="/settings/key/:key" element={<SettingDetailPage />} />
      <Route path="/settings/provider/:name" element={<Where />} />
    </Routes>,
    {
      daemon,
      route,
      wrapper: (children) => (
        <ThemeProvider>
          <SettingsProvider>{children}</SettingsProvider>
        </ThemeProvider>
      ),
    },
  );
}

const click = (element: HTMLElement) => act(() => element.click());

describe("SettingDetailPage for the names", () => {
  const nameInput = () => screen.getByLabelText("Agent name") as HTMLInputElement;
  const userInput = () => screen.getByLabelText("Your name") as HTMLInputElement;
  const save = () => screen.getByText("Save").closest("button") as HTMLButtonElement;
  const writes = (daemon: ReturnType<typeof fakeDaemon>) =>
    daemon.calls
      .filter((c) => c.method === "SetSetting" || c.method === "UnsetSetting")
      .map((c) => ({ method: c.method, request: c.request }));

  it("shows Borg while unset, and saves only a real change", async () => {
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/name", daemon);
    await waitFor(() => expect(nameInput().value).toBe("Borg"));
    expect(save().disabled).toBe(true);
    fireEvent.change(nameInput(), { target: { value: " Ada " } });
    fireEvent.keyDown(nameInput(), { key: "Enter" });
    await waitFor(() =>
      expect(writes(daemon)).toEqual([
        { method: "SetSetting", request: { key: "user.agent_name", value: "Ada" } },
      ]),
    );
  });

  it("rerolls from the daemon's pool into the field, then Save writes it", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: [entry("user.agent_name", "Borgzilla", "db")] },
      GetSetupStatus: setupStatus(),
      SetSetting: {},
    });
    mount("/settings/name", daemon);
    await waitFor(() => expect(nameInput().value).toBe("Borgzilla"));
    // The pool arrives with setup status; until then there is nothing to reroll to.
    const shuffle = screen.getByLabelText("Pick another name") as HTMLButtonElement;
    await waitFor(() => expect(shuffle.disabled).toBe(false));
    click(shuffle);
    await waitFor(() => expect(nameInput().value).toBe("Borg Vader"));
    expect(writes(daemon)).toEqual([]);
    click(save());
    await waitFor(() =>
      expect(writes(daemon)).toEqual([
        { method: "SetSetting", request: { key: "user.agent_name", value: "Borg Vader" } },
      ]),
    );
  });

  it("puts a cleared agent name back to Borg", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: [entry("user.agent_name", "Borgzilla", "db")] },
      GetSetupStatus: setupStatus(),
      UnsetSetting: {},
    });
    mount("/settings/name", daemon);
    await waitFor(() => expect(nameInput().value).toBe("Borgzilla"));
    fireEvent.change(nameInput(), { target: { value: "  " } });
    expect(nameInput().placeholder).toBe("Borg");
    click(save());
    await waitFor(() =>
      expect(writes(daemon)).toEqual([
        { method: "UnsetSetting", request: { key: "user.agent_name" } },
      ]),
    );
  });

  it("leaves a cleared default alone: there is nothing to put back", async () => {
    mount("/settings/name");
    await waitFor(() => expect(nameInput().value).toBe("Borg"));
    fireEvent.change(nameInput(), { target: { value: "" } });
    expect(save().disabled).toBe(true);
  });

  it("saves the user's name beside the agent's, and only that one when it alone changed", async () => {
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/name", daemon);
    await waitFor(() => expect(userInput().value).toBe(""));
    expect(userInput().placeholder).toBe("What should Borg call you?");
    fireEvent.change(userInput(), { target: { value: " Ada Lovelace " } });
    fireEvent.keyDown(userInput(), { key: "Enter" });
    await waitFor(() =>
      expect(writes(daemon)).toEqual([
        { method: "SetSetting", request: { key: "user.name", value: "Ada Lovelace" } },
      ]),
    );
  });

  it("writes both names with one Save", async () => {
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/name", daemon);
    await waitFor(() => expect(userInput().value).toBe(""));
    fireEvent.change(nameInput(), { target: { value: "Borgzilla" } });
    fireEvent.change(userInput(), { target: { value: "Ada" } });
    click(save());
    await waitFor(() =>
      expect(writes(daemon)).toEqual([
        { method: "SetSetting", request: { key: "user.agent_name", value: "Borgzilla" } },
        { method: "SetSetting", request: { key: "user.name", value: "Ada" } },
      ]),
    );
  });

  it("forgets the user's name when the field is cleared", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: [entry("user.agent_name", ""), entry("user.name", "Ada", "db")] },
      GetSetupStatus: setupStatus(),
      UnsetSetting: {},
    });
    mount("/settings/name", daemon);
    await waitFor(() => expect(userInput().value).toBe("Ada"));
    fireEvent.change(userInput(), { target: { value: "" } });
    click(save());
    await waitFor(() =>
      expect(writes(daemon)).toEqual([{ method: "UnsetSetting", request: { key: "user.name" } }]),
    );
  });

  it("stops at the first refusal, so the error on screen is the one that matters", async () => {
    const daemon = fakeDaemon({
      ...DAEMON,
      SetSetting: new BorgError(INVALID_ARGUMENT, "keep the name to 40 characters"),
    });
    mount("/settings/name", daemon);
    await waitFor(() => expect(userInput().value).toBe(""));
    fireEvent.change(nameInput(), { target: { value: "Borgzilla" } });
    fireEvent.change(userInput(), { target: { value: "Ada" } });
    click(save());
    await waitFor(() => expect(screen.getByText("keep the name to 40 characters")).toBeTruthy());
    expect(writes(daemon)).toEqual([
      { method: "SetSetting", request: { key: "user.agent_name", value: "Borgzilla" } },
    ]);
  });

  it("offers only the agent's name to a daemon that has no setting for the user's", async () => {
    mount(
      "/settings/name",
      fakeDaemon({
        GetSettings: { entries: [entry("user.agent_name", "")] },
        GetSetupStatus: setupStatus(),
      }),
    );
    await waitFor(() => expect(nameInput().value).toBe("Borg"));
    expect(screen.queryByLabelText("Your name")).toBeNull();
  });
});

describe("SettingDetailPage for a curated choice", () => {
  it("marks the option that is currently in force", async () => {
    mount("/settings/thinking");
    await waitFor(() => expect(screen.getByText("Thinking level")).toBeTruthy());
    const selected = screen.getByRole("img", { name: "Selected" });
    const row = selected.closest("button");
    expect(row?.textContent).toContain("Medium");
    expect(row?.getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: /^High/ }).getAttribute("aria-pressed")).toBe(
      "false",
    );
  });

  it("writes the picked option straight through", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/thinking", daemon);
    await waitFor(() => expect(screen.getByText("High")).toBeTruthy());

    click(screen.getByText("High"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "llm.thinking",
        value: "high",
      }),
    );
  });

  it("does not write when the option picked is the one already set", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/thinking", daemon);
    await waitFor(() => expect(screen.getByText("Medium")).toBeTruthy());

    click(screen.getByText("Medium"));
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });

  it("shows why the daemon rejected a choice", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: REGISTRY },
      SetSetting: new BorgError(INVALID_ARGUMENT, "unknown thinking level"),
    });
    mount("/settings/thinking", daemon);
    await waitFor(() => expect(screen.getByText("High")).toBeTruthy());

    click(screen.getByText("High"));
    await waitFor(() => expect(screen.getByText("unknown thinking level")).toBeTruthy());
  });
});

describe("SettingDetailPage for a free-text value", () => {
  it("opens with the effective value and saves an edit", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());

    fireEvent.change(screen.getByDisplayValue("7842"), { target: { value: "9000" } });
    click(screen.getByText("Save"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "gateway.port",
        value: "9000",
      }),
    );
  });

  it("saves on Enter, the way a single-field form should", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());

    fireEvent.keyDown(screen.getByDisplayValue("7842"), { key: "Enter" });
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(true));
  });

  it("follows the daemon's normalised value after a save", async () => {
    // The daemon may store something other than what was typed.
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());

    daemon.answer("GetSettings", {
      entries: [...REGISTRY.slice(0, 3), entry("gateway.port", "9001")],
    });
    fireEvent.change(screen.getByDisplayValue("7842"), { target: { value: " 9001 " } });
    click(screen.getByText("Save"));
    await waitFor(() => expect(screen.getByDisplayValue("9001")).toBeTruthy());
  });

  it("offers a reset only for a key that was actually overridden", async () => {
    // Resetting a compiled default would be a control that does nothing.
    mount("/settings/key/gateway.port");
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());
    expect(screen.queryByText("Reset to default")).toBeNull();
  });

  it("clears an overridden key through Unset", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, UnsetSetting: {} });
    mount("/settings/key/agents.max_depth", daemon);
    await waitFor(() => expect(screen.getByText("Reset to default")).toBeTruthy());

    click(screen.getByText("Reset to default"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "UnsetSetting")?.request).toEqual({
        key: "agents.max_depth",
      }),
    );
  });

  it("decodes a key that had to be escaped into the URL", async () => {
    mount(`/settings/key/${encodeURIComponent("agents.max_depth")}`);
    await waitFor(() => expect(screen.getByDisplayValue("3")).toBeTruthy());
  });
});

describe("SettingDetailPage for a count", () => {
  const COUNT = [entry("llm.request_timeout_ms", "120000", "db")];

  it("shows the thousands grouped and saves the plain number", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: COUNT }, SetSetting: {} });
    mount("/settings/key/llm.request_timeout_ms", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("120,000")).toBeTruthy());

    fireEvent.change(screen.getByDisplayValue("120,000"), { target: { value: "1,500,000" } });
    click(screen.getByText("Save"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "llm.request_timeout_ms",
        value: "1500000",
      }),
    );
  });

  it("leaves the digits of a number the daemon calls text alone", async () => {
    // A Vertex project number is a name; `123,456,789,012` is nobody's project.
    const project = { ...entry("llm.vertex_project", "123456789012", "db"), kind: "text" };
    mount("/settings/key/llm.vertex_project", fakeDaemon({ GetSettings: { entries: [project] } }));
    await waitFor(() => expect(screen.getByDisplayValue("123456789012")).toBeTruthy());
  });
});

describe("SettingDetailPage for a JSON value", () => {
  const JSON_REGISTRY = [
    entry("security.blocked_paths", '[".ssh",".aws"]', "db"),
    entry("security.action_limits", '{"tool_calls_warn":200,"tool_calls_block":500}'),
  ];
  const box = () => screen.getByLabelText(/security\./) as HTMLTextAreaElement;
  const daemonWith = (extra = {}) =>
    fakeDaemon({ GetSettings: { entries: JSON_REGISTRY }, ...extra });

  it("lays a list out one entry per line in a multi-line box", async () => {
    mount("/settings/key/security.blocked_paths", daemonWith());
    await waitFor(() => expect(box().value).toBe('[\n  ".ssh",\n  ".aws"\n]'));
    expect(box().tagName).toBe("TEXTAREA");
  });

  it("pretty-prints an object too", async () => {
    mount("/settings/key/security.action_limits", daemonWith());
    await waitFor(() => expect(box().value).toContain('\n  "tool_calls_warn": 200,'));
  });

  it("saves the compact form the daemon stores", async () => {
    const daemon = daemonWith({ SetSetting: {} });
    mount("/settings/key/security.blocked_paths", daemon);
    await waitFor(() => expect(box().value).toContain(".ssh"));

    fireEvent.change(box(), { target: { value: '[\n  ".ssh",\n  ".kube"\n]' } });
    click(screen.getByText("Save"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "security.blocked_paths",
        value: '[".ssh",".kube"]',
      }),
    );
  });

  it("stops a draft that does not parse before it reaches the daemon", async () => {
    const daemon = daemonWith({ SetSetting: {} });
    mount("/settings/key/security.blocked_paths", daemon);
    await waitFor(() => expect(box().value).toContain(".ssh"));

    fireEvent.change(box(), { target: { value: '[".ssh",' } });
    click(screen.getByText("Save"));
    expect(screen.getByRole("alert").textContent).toMatch(/not valid JSON/);
    expect(box().getAttribute("aria-invalid")).toBe("true");
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);

    // Typing again clears the complaint.
    fireEvent.change(box(), { target: { value: '[".ssh"]' } });
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("stops a list saved as something that is not a list", async () => {
    // Valid JSON, but the daemon would refuse an object for a list with a
    // serde trace; the box says it in words instead.
    const daemon = daemonWith({ SetSetting: {} });
    mount("/settings/key/security.blocked_paths", daemon);
    await waitFor(() => expect(box().value).toContain(".ssh"));

    fireEvent.change(box(), { target: { value: '{"path": ".ssh"}' } });
    click(screen.getByText("Save"));
    expect(screen.getByRole("alert").textContent).toMatch(/is a list/);
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });

  it("keeps Enter as a new line and saves on Cmd or Ctrl+Enter", async () => {
    const daemon = daemonWith({ SetSetting: {} });
    mount("/settings/key/security.blocked_paths", daemon);
    await waitFor(() => expect(box().value).toContain(".ssh"));

    fireEvent.keyDown(box(), { key: "Enter" });
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);

    fireEvent.keyDown(box(), { key: "Enter", ctrlKey: true });
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(true));
  });

  it("resets an overridden list to its default", async () => {
    const daemon = daemonWith({ UnsetSetting: {} });
    mount("/settings/key/security.blocked_paths", daemon);
    await waitFor(() => expect(screen.getByText("Reset to default")).toBeTruthy());
    click(screen.getByText("Reset to default"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "UnsetSetting")?.request).toEqual({
        key: "security.blocked_paths",
      }),
    );
  });
});

describe("SettingDetailPage edge cases", () => {
  it("says so for a route segment that names no setting", () => {
    mount("/settings/nonsense");
    expect(screen.getByText("That setting doesn’t exist.")).toBeTruthy();
  });

  it("says so for a key the registry does not hold", async () => {
    mount("/settings/key/not.a.real.key");
    await waitFor(() => expect(screen.getByText("That setting doesn’t exist.")).toBeTruthy());
  });

  it("reports a failed registry load rather than an empty editor", async () => {
    const daemon = fakeDaemon({
      GetSettings: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByText("Could not load settings.")).toBeTruthy());
  });
});

describe("SettingDetailPage for provider and model", () => {
  it("offers the daemon's providers by name and writes the id", async () => {
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    const selected = screen.getByRole("img", { name: "Selected" });
    expect(selected.closest("button")?.textContent).toContain("Anthropic");

    click(screen.getByText("OpenRouter"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "provider",
        value: "openrouter",
      }),
    );
  });

  it("moves to the new provider's default model when it does not list the current one", async () => {
    // claude-opus-5 is an Anthropic id; OpenRouter would be sent a model it
    // does not carry under that name.
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    click(screen.getByText("OpenRouter"));
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
        { key: "provider", value: "openrouter" },
        { key: "model", value: "deepseek/deepseek-v4-flash" },
      ]),
    );
  });

  it("leaves the model alone when the provider write fails", async () => {
    const daemon = fakeDaemon({
      ...DAEMON,
      SetSetting: new BorgError(GrpcStatus.Internal, "disk full"),
    });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    click(screen.getByText("OpenRouter"));
    await waitFor(() => expect(screen.getByText(/disk full/)).toBeTruthy());
    expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
      { key: "provider", value: "openrouter" },
    ]);
  });

  it("says so when the provider switched but its model could not follow", async () => {
    // Two writes: the provider lands, the model does not. OpenRouter is now
    // paired with an Anthropic model id, and the user has to know that.
    const daemon = fakeDaemon({
      ...DAEMON,
      SetSetting: (req: unknown) =>
        (req as { key: string }).key === "model"
          ? new BorgError(GrpcStatus.Internal, "disk full")
          : {},
    });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    click(screen.getByText("OpenRouter"));

    await waitFor(() =>
      expect(
        screen.getByText(/Now using OpenRouter, but its model could not be switched/),
      ).toBeTruthy(),
    );
    expect(screen.getByRole("link", { name: "Pick a model" }).getAttribute("href")).toBe(
      "/settings/model",
    );
  });

  it("lists only the models of the provider in force", async () => {
    mount("/settings/model");
    await waitFor(() => expect(screen.getByText("Opus 5")).toBeTruthy());
    expect(screen.getByText("Sonnet 5")).toBeTruthy();
    expect(screen.queryByText("DeepSeek V4 Flash")).toBeNull();
  });

  it("marks the provider default as selected when no model is set", async () => {
    const daemon = fakeDaemon({
      ...DAEMON,
      GetSettings: { entries: [entry("provider", "anthropic"), entry("model", "")] },
    });
    mount("/settings/model", daemon);
    await waitFor(() => expect(screen.getByText("Opus 5")).toBeTruthy());
    const selected = screen.getByRole("img", { name: "Selected" });
    expect(selected.closest("button")?.textContent).toContain("Opus 5");
  });

  it("keeps free text one step away, for an id the catalog does not carry", async () => {
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/model", daemon);
    await waitFor(() => expect(screen.getByText("Something else…")).toBeTruthy());
    expect(screen.queryByDisplayValue("claude-opus-5")).toBeNull();

    click(screen.getByText("Something else…"));
    fireEvent.change(screen.getByDisplayValue("claude-opus-5"), {
      target: { value: "claude-opus-6-preview" },
    });
    click(screen.getByText("Save"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "model",
        value: "claude-opus-6-preview",
      }),
    );
  });

  it("opens straight into text for a value the catalog does not list", async () => {
    const daemon = fakeDaemon({
      ...DAEMON,
      GetSettings: { entries: [entry("provider", "anthropic"), entry("model", "my-finetune")] },
    });
    mount("/settings/model", daemon);
    // The text editor can appear before the catalog lands (no options yet
    // means text); the way back exists only once the catalog has answered.
    await waitFor(() => expect(screen.getByText("Back to the list")).toBeTruthy());
    expect(screen.getByDisplayValue("my-finetune")).toBeTruthy();
    click(screen.getByText("Back to the list"));
    expect(screen.getByText("Opus 5")).toBeTruthy();
  });

  it("falls back to text when the catalog cannot answer", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: REGISTRY },
      GetSetupStatus: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("anthropic")).toBeTruthy());
    expect(screen.queryByText("Back to the list")).toBeNull();
  });
});

/** A catalog row for a provider the shared fixture leaves out. */
const row = (id: string, label: string, over: Record<string, unknown> = {}) => ({
  id,
  label,
  blurb: "",
  keyUrl: "",
  needsApiKey: false,
  recommended: false,
  featured: false,
  firstRun: false,
  defaultModel: "",
  models: [],
  ...over,
});

const CATALOG = [
  ...SETUP_CATALOG,
  row("bedrock", "AWS Bedrock", { firstRun: true, defaultModel: "claude-opus-5" }),
  row("claude-cli", "Claude Code CLI", { firstRun: true, defaultModel: "opus" }),
  row("codex-oauth", "Codex", { needsApiKey: true, defaultModel: "gpt-5" }),
  row("custom", "Custom endpoint", { needsApiKey: true }),
];

/** The Provider page with `provider` in force and the daemon answering `over`. */
function providerPage(provider: string, over: Record<string, unknown> = {}, registry = REGISTRY) {
  const daemon = fakeDaemon({
    ...DAEMON,
    GetSettings: {
      entries: registry.map((e) => (e.key === "provider" ? { ...e, value: provider } : e)),
    },
    GetSetupStatus: setupStatus({ providers: CATALOG, provider }),
    ...over,
  });
  mount("/settings/provider", daemon);
  return daemon;
}

describe("SettingDetailPage: what the provider in force needs", () => {
  it("says when the chosen provider has no key, and opens its key page", async () => {
    providerPage("anthropic");
    await waitFor(() => expect(screen.getByText("Set up Anthropic")).toBeTruthy());
    const keyRow = screen.getByRole("button", { name: /^API key/ });
    expect(keyRow.textContent).toContain("None saved");
    click(keyRow);
    expect(screen.getByTestId("path").textContent).toBe("/settings/provider/anthropic");
  });

  it("names a saved key instead of asking for one", async () => {
    providerPage("anthropic", {
      ListApiKeys: { entries: [{ provider: "anthropic", backend: "keychain" }] },
    });
    await waitFor(() => expect(screen.getByRole("button", { name: /^API key/ })).toBeTruthy());
    expect(screen.getByRole("button", { name: /^API key/ }).textContent).toContain("Saved");
    expect(screen.queryByText("None saved")).toBeNull();
  });

  it("asks Bedrock for its region and endpoint, not a key, and comes back to Provider", async () => {
    const registry = [
      ...REGISTRY,
      { ...entry("llm.bedrock_region", "us-east-1", "db"), label: "Bedrock region" },
      { ...entry("llm.bedrock_endpoint_url", ""), label: "Bedrock endpoint URL" },
      { ...entry("llm.bedrock_use_fips", "false"), label: "Bedrock FIPS endpoints", kind: "bool" },
    ];
    providerPage("bedrock", {}, registry);
    await waitFor(() => expect(screen.getByText("Set up AWS Bedrock")).toBeTruthy());
    expect(screen.getByText(/AWS credentials on this machine/)).toBeTruthy();
    expect(screen.getByRole("button", { name: /^Bedrock region/ }).textContent).toContain(
      "us-east-1",
    );
    expect(screen.getByRole("button", { name: /^Bedrock endpoint URL/ }).textContent).toContain(
      "Not set",
    );
    expect(screen.getByRole("button", { name: /^Bedrock FIPS endpoints/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^API key/ })).toBeNull();

    click(screen.getByRole("button", { name: /^Bedrock region/ }));
    await waitFor(() => expect(screen.getByDisplayValue("us-east-1")).toBeTruthy());
    // The editor leads back to where it was opened from, not to Settings.
    expect(screen.getByText("Provider")).toBeTruthy();
    expect(screen.queryByText(/^Settings$/)).toBeNull();
  });

  it("says whether Claude Code is signed in on this machine", async () => {
    providerPage("claude-cli", {
      GetSetupStatus: setupStatus({
        providers: CATALOG,
        provider: "claude-cli",
        detected: [
          { provider: "claude-cli", source: "claude_cli", detail: "/usr/local/bin/claude" },
        ],
      }),
    });
    await waitFor(() =>
      expect(
        screen.getByText("Signed in. Borg runs Claude Code from /usr/local/bin/claude."),
      ).toBeTruthy(),
    );
  });

  it("says what to do when Claude Code is not signed in", async () => {
    providerPage("claude-cli");
    await waitFor(() =>
      expect(screen.getByText(/Claude Code is not signed in on this machine/)).toBeTruthy(),
    );
  });

  it("imports a sign-in into the provider that runs on it", async () => {
    const daemon = providerPage("codex-oauth", { PairCredential: { entries: [{ id: "o1" }] } });
    await waitFor(() => expect(screen.getByText("Import from Codex")).toBeTruthy());
    const listed = daemon.calls.filter((c) => c.method === "ListCredentials").length;
    click(screen.getByText("Import from Codex"));
    await waitFor(() => expect(screen.getByText("Imported 1 account.")).toBeTruthy());
    expect(daemon.calls.find((c) => c.method === "PairCredential")?.request).toEqual({
      kind: "codex",
    });
    // The pool it landed in is read again, so the keys row is current.
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "ListCredentials").length).toBe(listed + 1),
    );
  });

  it("says what to do when there is no sign-in to import", async () => {
    providerPage("codex-oauth", { PairCredential: { entries: [] } });
    await waitFor(() => expect(screen.getByText("Import from Codex")).toBeTruthy());
    click(screen.getByText("Import from Codex"));
    await waitFor(() =>
      expect(
        screen.getByText("Nothing found on this machine — sign in with Codex first."),
      ).toBeTruthy(),
    );
  });

  it("reports a failed import rather than looking like it did nothing", async () => {
    providerPage("codex-oauth", {
      PairCredential: new BorgError(GrpcStatus.NotFound, "no credentials file"),
    });
    await waitFor(() => expect(screen.getByText("Import from Codex")).toBeTruthy());
    click(screen.getByText("Import from Codex"));
    await waitFor(() => expect(screen.getByText("no credentials file")).toBeTruthy());
  });

  it("keeps every other provider's keys in reach, and flags a pool that stopped working", async () => {
    providerPage("anthropic", {
      ListCredentials: {
        providers: [
          {
            provider: "openrouter",
            strategy: "fill_first",
            cooldownSecs: 60,
            entries: [{ id: "c1", label: "k", status: "exhausted", fingerprint: "", useCount: 0 }],
          },
        ],
      },
    });
    await waitFor(() => expect(screen.getByText("Keys for other providers")).toBeTruthy());
    const section = screen.getByText("Keys for other providers").closest("section")!;
    const other = within(section).getByRole("button", { name: /^OpenRouter/ });
    expect(other.textContent).toContain("1 key");
    expect(within(other).getByRole("img", { name: "Keys not working" })).toBeTruthy();
    click(other);
    expect(screen.getByTestId("path").textContent).toBe("/settings/provider/openrouter");
  });

  it("asks for the model when the new provider has no list to take one from", async () => {
    const daemon = providerPage("anthropic", { SetSetting: {} });
    await waitFor(() => expect(screen.getByText("Custom endpoint")).toBeTruthy());
    click(screen.getByText("Custom endpoint"));
    await waitFor(() =>
      expect(
        screen.getByText(/Now using Custom endpoint. Name the model it should run./),
      ).toBeTruthy(),
    );
    // Anthropic's model id would only be wrong on someone else's server.
    expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
      { key: "provider", value: "custom" },
    ]);
    expect((screen.getByText("Set the model") as HTMLAnchorElement).getAttribute("href")).toBe(
      "/settings/model",
    );
  });
});

describe("appearance, which never reaches the daemon", () => {
  it("applies a theme change immediately and stores it locally", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY } });
    mount("/settings/appearance", daemon);

    click(screen.getByText("Light"));
    await waitFor(() => expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe("light"));
    // The theme must not become a daemon setting: it has to apply before paint.
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });

  it("ignores a pick of the theme already in force", async () => {
    mount("/settings/appearance");
    click(screen.getByText("Light"));
    await waitFor(() => expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe("light"));
    click(screen.getByText("Light"));
    expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe("light");
  });
});

/** The two shapes an input method's Enter takes; neither one means "submit". */
const IME_ENTERS: [string, { key: string; isComposing?: boolean; keyCode?: number }][] = [
  ["while a word is still being composed", { key: "Enter", isComposing: true }],
  ["that confirms a candidate after composition ended", { key: "Enter", keyCode: 229 }],
];

describe("SettingDetailPage and an input method", () => {
  it.each(IME_ENTERS)("does not save on an Enter %s", async (_name, init) => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());

    fireEvent.keyDown(screen.getByDisplayValue("7842"), init);
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });

    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });
});
