import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp, type UnaryTable } from "../../test/harness";
import { SETUP_CATALOG, setupStatus } from "../../test/setupStatus";
import { OnboardingPage } from "./OnboardingPage";
import { detectionNote, leadDetection } from "./useSetupStatus";

afterEach(cleanup);

const WRITES = { SetApiKey: {}, SetSetting: {}, SetOnboardingComplete: {} };

function mount(table: UnaryTable, onDone = vi.fn()) {
  const daemon = fakeDaemon({ ...WRITES, ...table });
  renderApp(<OnboardingPage onDone={onDone} />, { daemon });
  return { daemon, onDone };
}

const start = () => screen.getByText("Start talking") as HTMLButtonElement;
const methods = (daemon: ReturnType<typeof fakeDaemon>) => daemon.calls.map((c) => c.method);

describe("OnboardingPage with nothing on the machine", () => {
  it("preselects the recommended provider and asks for one key", async () => {
    const { daemon } = mount({ GetSetupStatus: setupStatus() });
    await waitFor(() => expect(screen.getByPlaceholderText("OpenRouter API key")).toBeTruthy());
    // Featured rows show; the fold hides the rest until asked.
    expect(screen.getByText("Anthropic")).toBeTruthy();
    expect(screen.queryByText("Groq")).toBeNull();
    fireEvent.click(screen.getByText("More providers"));
    expect(screen.getByText("Groq")).toBeTruthy();
    expect(daemon.calls.filter((c) => c.method === "GetSetupStatus")).toHaveLength(1);
  });

  it("says where the key comes from, using the daemon's link", async () => {
    // Picking a provider and being asked for a credential with no hint where
    // to get it is the one dead end a first run cannot recover from.
    mount({ GetSetupStatus: setupStatus() });
    await waitFor(() => expect(screen.getByText("Anthropic")).toBeTruthy());
    fireEvent.click(screen.getByText("Anthropic"));
    const link = screen.getByText("Get a key from Anthropic ↗") as HTMLAnchorElement;
    expect(link.getAttribute("href")).toBe("https://console.anthropic.com/settings/keys");
  });

  it("holds back until a key has been pasted, then writes key, provider, model, done", async () => {
    const { daemon, onDone } = mount({ GetSetupStatus: setupStatus() });
    await waitFor(() => expect(start().disabled).toBe(true));
    fireEvent.change(screen.getByPlaceholderText("OpenRouter API key"), {
      target: { value: "  sk-or-test  " },
    });
    expect(start().disabled).toBe(false);
    fireEvent.click(start());
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    // The key lands before the provider that needs it, and "done" comes last.
    expect(methods(daemon).filter((m) => m !== "GetSetupStatus")).toEqual([
      "SetApiKey",
      "SetSetting",
      "SetSetting",
      "SetOnboardingComplete",
    ]);
    expect(daemon.calls.find((c) => c.method === "SetApiKey")?.request).toEqual({
      provider: "openrouter",
      literal: "sk-or-test",
    });
    expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
      { key: "provider", value: "openrouter" },
      { key: "model", value: "deepseek/deepseek-v4-flash" },
    ]);
  });

  it("chooses the model but lets it be changed inline", async () => {
    const { daemon } = mount({ GetSetupStatus: setupStatus() });
    await waitFor(() => expect(screen.getByText("DeepSeek V4 Flash")).toBeTruthy());
    expect(screen.queryByText("Claude Opus 5")).toBeNull();
    fireEvent.click(screen.getByText("Change"));
    fireEvent.click(screen.getByText("Claude Opus 5"));
    fireEvent.change(screen.getByPlaceholderText("OpenRouter API key"), {
      target: { value: "k" },
    });
    fireEvent.keyDown(screen.getByPlaceholderText("OpenRouter API key"), { key: "Enter" });
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "SetOnboardingComplete")).toBe(true),
    );
    expect(daemon.calls.map((c) => c.request)).toContainEqual({
      key: "model",
      value: "anthropic/claude-opus-5",
    });
  });

  it("asks for nothing when the model runs on this machine", async () => {
    const { daemon } = mount({ GetSetupStatus: setupStatus() });
    await waitFor(() => expect(screen.getByText("More providers")).toBeTruthy());
    fireEvent.click(screen.getByText("More providers"));
    fireEvent.click(screen.getByText("Ollama"));
    expect(screen.queryByPlaceholderText(/API key/)).toBeNull();
    expect(screen.queryByText(/Get a key from/)).toBeNull();
    expect(start().disabled).toBe(false);
    fireEvent.click(start());
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "SetOnboardingComplete")).toBe(true),
    );
    expect(methods(daemon)).not.toContain("SetApiKey");
  });

  it("does not mark onboarding done when a write fails", async () => {
    const { daemon, onDone } = mount({
      GetSetupStatus: setupStatus(),
      SetApiKey: new BorgError(GrpcStatus.Internal, "disk full"),
    });
    await waitFor(() => expect(screen.getByPlaceholderText("OpenRouter API key")).toBeTruthy());
    fireEvent.change(screen.getByPlaceholderText("OpenRouter API key"), {
      target: { value: "k" },
    });
    fireEvent.click(start());
    await waitFor(() => expect(screen.getByText(/Could not save that: disk full/)).toBeTruthy());
    expect(methods(daemon)).not.toContain("SetOnboardingComplete");
    expect(onDone).not.toHaveBeenCalled();
  });

  it("lets the user skip with nothing stored", async () => {
    const { daemon, onDone } = mount({ GetSetupStatus: setupStatus() });
    await waitFor(() => expect(screen.getByText("I’ll do it later")).toBeTruthy());
    fireEvent.click(screen.getByText("I’ll do it later"));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(methods(daemon).filter((m) => m !== "GetSetupStatus")).toEqual([
      "SetOnboardingComplete",
    ]);
  });
});

describe("OnboardingPage with something already here", () => {
  it("offers a found key with one click and never asks to paste it", async () => {
    const { daemon, onDone } = mount({
      GetSetupStatus: setupStatus({
        detected: [{ provider: "anthropic", source: "dotenv", detail: "/home/me/.borg/.env" }],
      }),
    });
    await waitFor(() =>
      expect(screen.getByText(/Found your Anthropic key in \/home\/me\/.borg\/.env/)).toBeTruthy(),
    );
    expect(screen.queryByPlaceholderText(/API key/)).toBeNull();
    fireEvent.click(screen.getByText("Use Anthropic"));
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(methods(daemon)).not.toContain("SetApiKey");
    expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
      { key: "provider", value: "anthropic" },
      { key: "model", value: "claude-opus-5" },
    ]);
  });

  it("still lets the user pick something else", async () => {
    mount({
      GetSetupStatus: setupStatus({
        detected: [{ provider: "ollama", source: "ollama", detail: "http://localhost:11434" }],
      }),
    });
    await waitFor(() => expect(screen.getByText(/Ollama is running/)).toBeTruthy());
    fireEvent.click(screen.getByText("Use something else"));
    expect(screen.getByPlaceholderText("OpenRouter API key")).toBeTruthy();
  });

  it("finishes in one click when a provider is already configured", async () => {
    const { daemon, onDone } = mount({
      GetSetupStatus: setupStatus({ configured: true, provider: "openrouter", model: "x" }),
    });
    await waitFor(() => expect(screen.getByText(/already set up with OpenRouter/)).toBeTruthy());
    fireEvent.click(start());
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(methods(daemon).filter((m) => m !== "GetSetupStatus")).toEqual([
      "SetOnboardingComplete",
    ]);
  });
});

describe("OnboardingPage when the daemon cannot answer", () => {
  it("offers a retry and a way out rather than a blank screen", async () => {
    const daemon = fakeDaemon({
      ...WRITES,
      GetSetupStatus: new BorgError(GrpcStatus.Unavailable, "socket gone"),
    });
    const onDone = vi.fn();
    renderApp(<OnboardingPage onDone={onDone} />, { daemon });
    await waitFor(() => expect(screen.getByText("socket gone")).toBeTruthy());
    daemon.answer("GetSetupStatus", setupStatus());
    fireEvent.click(screen.getByText("Try again"));
    await waitFor(() => expect(screen.getByPlaceholderText("OpenRouter API key")).toBeTruthy());
  });
});

describe("leadDetection", () => {
  const det = (provider: string, source: string) => ({ provider, source, detail: "" });

  it("prefers the configured provider's own detection over a better-ranked one", () => {
    const lead = leadDetection(
      setupStatus({ provider: "groq", detected: [det("openai", "stored"), det("groq", "env")] }),
    );
    expect(lead?.provider).toBe("groq");
  });

  it("otherwise leads with what Borg already holds, then env, then runtimes", () => {
    const lead = leadDetection(
      setupStatus({
        detected: [det("ollama", "ollama"), det("openai", "env"), det("groq", "stored")],
      }),
    );
    expect(lead?.provider).toBe("groq");
    expect(leadDetection(setupStatus())).toBeUndefined();
  });

  it("never leads with a source this build does not know", () => {
    // A tier added on the daemon side must not outrank a saved key.
    const lead = leadDetection(
      setupStatus({ detected: [det("openai", "brand-new-tier"), det("groq", "stored")] }),
    );
    expect(lead?.provider).toBe("groq");
  });

  it("describes a detection without repeating the secret", () => {
    expect(detectionNote(SETUP_CATALOG, det("groq", "stored"))).toBe(
      "Your Groq key is already saved.",
    );
    expect(
      detectionNote(SETUP_CATALOG, {
        provider: "openrouter",
        source: "env",
        detail: "OPENROUTER_API_KEY",
      }),
    ).toBe("Found your OpenRouter key in OPENROUTER_API_KEY.");
  });
});
