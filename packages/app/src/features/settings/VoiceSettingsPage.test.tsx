import { describe, expect, it } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";
import { ThemeProvider } from "@borg/ui";
import { installMatchMedia } from "@borg/ui/test/matchMedia";
import { fakeDaemon, renderApp } from "../../test/harness";
import { SettingsProvider } from "../../lib/settings";
import { VoiceSettingsPage } from "./VoiceSettingsPage";

installMatchMedia(() => false);

const entry = (key: string, value: string) => ({
  key,
  value,
  category: "db",
  label: "",
  kind: "",
  group: "",
});

const provider = (id: string, label: string, extra: Record<string, unknown> = {}) => ({
  id,
  label,
  needsKey: true,
  keyUrl: `https://${id}.example/keys`,
  defaultModel: "",
  defaultVoice: "",
  requiresCommand: false,
  hasKey: false,
  ...extra,
});

const CATALOG = {
  speech: [
    provider("elevenlabs", "ElevenLabs", { defaultVoice: "adam", defaultModel: "eleven_v2" }),
    provider("edge", "Microsoft Edge (free)", { needsKey: false, hasKey: true, keyUrl: "" }),
  ],
  transcription: [
    provider("openai", "OpenAI", { defaultModel: "whisper-1", hasKey: true }),
    provider("elevenlabs", "ElevenLabs Scribe", { defaultModel: "scribe_v1" }),
  ],
  phone: [
    {
      provider: provider("twilio", "Twilio (speak a message)"),
      callerIdLabel: "Twilio phone number",
      usesAgentId: false,
      agentRequired: false,
      usesAccountId: true,
    },
    {
      provider: provider("elevenlabs", "ElevenLabs voice agent"),
      callerIdLabel: "ElevenLabs phone number id",
      usesAgentId: true,
      agentRequired: true,
      usesAccountId: false,
    },
  ],
  speechReady: false,
  transcriptionReady: true,
  phoneProblem: "Phone calls are off.",
};

function mount(entries: ReturnType<typeof entry>[], catalog: unknown = CATALOG) {
  const daemon = fakeDaemon({
    GetSettings: { entries },
    GetVoiceCatalog: catalog,
    SetSetting: {},
    SetApiKey: {},
  });
  const view = renderApp(<VoiceSettingsPage />, {
    daemon,
    wrapper: (children) => (
      <ThemeProvider>
        <SettingsProvider>{children}</SettingsProvider>
      </ThemeProvider>
    ),
  });
  return { ...view, daemon };
}

const saved = (daemon: ReturnType<typeof mount>["daemon"], key: string) =>
  daemon.calls
    .filter((c) => c.method === "SetSetting")
    .map((c) => c.request as { key: string; value: string })
    .filter((r) => r.key === key)
    .map((r) => r.value);

describe("VoiceSettingsPage", () => {
  it("starts with each outcome behind one switch and no provider fields", async () => {
    mount([entry("audio.enabled", "false"), entry("tts.enabled", "false")]);
    const listen = await screen.findByRole("switch", { name: /Talk to Borg/ });
    expect(listen.getAttribute("aria-checked")).toBe("false");
    expect(screen.getByRole("switch", { name: /Let Borg speak/ })).toBeTruthy();
    // Only the phone section has a provider picker while both switches are off.
    expect(screen.getAllByLabelText("Provider")).toHaveLength(1);
  });

  it("turns a capability on through its setting", async () => {
    const { daemon } = mount([entry("audio.enabled", "false")]);
    fireEvent.click(await screen.findByRole("switch", { name: /Talk to Borg/ }));
    await waitFor(() => expect(saved(daemon, "audio.enabled")).toEqual(["true"]));
  });

  it("switching provider starts a fresh first row and keeps the fallbacks", async () => {
    const chain = JSON.stringify([
      { provider: "openai", model: "whisper-1" },
      { provider: "custom", command: "x {audio_file}" },
    ]);
    const { daemon } = mount([entry("audio.enabled", "true"), entry("audio.models", chain)]);
    const pickers = await screen.findAllByLabelText("Provider");
    const transcription = pickers[0] as HTMLSelectElement;
    expect(transcription.value).toBe("openai");
    fireEvent.change(transcription, { target: { value: "elevenlabs" } });
    await waitFor(() => expect(saved(daemon, "audio.models")).toHaveLength(1));
    expect(JSON.parse(saved(daemon, "audio.models")[0]!)).toEqual([
      { provider: "elevenlabs" },
      { provider: "custom", command: "x {audio_file}" },
    ]);
  });

  it("shows the provider's own default voice as the placeholder and saves an override", async () => {
    const { daemon } = mount([
      entry("tts.enabled", "true"),
      entry("tts.models", JSON.stringify([{ provider: "elevenlabs" }])),
    ]);
    const voice = (await screen.findByLabelText("Voice")) as HTMLInputElement;
    expect(voice.placeholder).toBe("adam");
    fireEvent.change(voice, { target: { value: "rachel" } });
    fireEvent.blur(voice);
    await waitFor(() =>
      expect(saved(daemon, "tts.models")).toEqual([
        JSON.stringify([{ provider: "elevenlabs", voice: "rachel" }]),
      ]),
    );
    expect(screen.getByRole("status").textContent).toMatch(/Not ready yet/);
  });

  it("saves a key to the provider's credential slot and asks the catalog again", async () => {
    const { daemon } = mount([
      entry("tts.enabled", "true"),
      entry("tts.models", JSON.stringify([{ provider: "elevenlabs" }])),
    ]);
    const key = await screen.findByLabelText("ElevenLabs API key");
    expect(screen.getByRole("link", { name: "Get a key" }).getAttribute("href")).toBe(
      "https://elevenlabs.example/keys",
    );
    fireEvent.change(key, { target: { value: "xi-123" } });
    fireEvent.keyDown(key, { key: "Enter" });
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetApiKey")?.request).toEqual({
        provider: "elevenlabs",
        literal: "xi-123",
      }),
    );
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "GetVoiceCatalog").length).toBeGreaterThan(1),
    );
  });

  it("a keyless provider asks for no key", async () => {
    mount([
      entry("tts.enabled", "true"),
      entry("tts.models", JSON.stringify([{ provider: "edge" }])),
    ]);
    await screen.findByLabelText("Voice");
    expect(screen.queryByLabelText(/API key/)).toBeNull();
  });

  it("phone shows only the fields the chosen provider reads, and why it is not ready", async () => {
    const { daemon } = mount(
      [entry("telephony.provider", "twilio"), entry("telephony.caller_id", "")],
      { ...CATALOG, phoneProblem: "Twilio needs your Account SID." },
    );
    expect(await screen.findByLabelText("Account SID")).toBeTruthy();
    expect(screen.queryByLabelText(/Agent id/)).toBeNull();
    expect(screen.getByText("Twilio needs your Account SID.")).toBeTruthy();

    const caller = screen.getByLabelText("Twilio phone number");
    fireEvent.change(caller, { target: { value: "+14155550123" } });
    fireEvent.keyDown(caller, { key: "Enter" });
    await waitFor(() => expect(saved(daemon, "telephony.caller_id")).toEqual(["+14155550123"]));
  });

  it("a voice-agent provider asks for its agent id", async () => {
    mount([entry("telephony.provider", "elevenlabs")]);
    expect(await screen.findByLabelText("Agent id")).toBeTruthy();
    expect(screen.queryByLabelText("Account SID")).toBeNull();
  });

  it("picking Off turns phone calls off", async () => {
    const { daemon } = mount([entry("telephony.provider", "twilio")]);
    await screen.findByLabelText("Account SID");
    const pickers = screen.getAllByLabelText("Provider");
    fireEvent.change(pickers[pickers.length - 1]!, { target: { value: "" } });
    await waitFor(() => expect(saved(daemon, "telephony.provider")).toEqual([""]));
  });
});
