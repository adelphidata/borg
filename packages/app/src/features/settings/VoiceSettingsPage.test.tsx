import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { ThemeProvider } from "@borg/ui";
import { installMatchMedia } from "@borg/ui/test/matchMedia";
import { fakeDaemon, renderApp } from "../../test/harness";
import { BorgError, GrpcStatus } from "@borg/client";
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
  keyLabel: `${label} API key`,
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
      provider: provider("twilio", "Twilio (speak a message)", { keyLabel: "Twilio Auth Token" }),
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
    expect(
      screen.getByRole("switch", { name: /Read replies aloud/ }).getAttribute("aria-checked"),
    ).toBe("false");
    // Only the phone section has a provider picker while voice input is off;
    // spoken replies default to the device's own voice.
    expect(screen.getAllByLabelText("Provider")).toHaveLength(1);
    expect((screen.getByLabelText("Voice") as HTMLSelectElement).value).toBe("");
    expect(screen.queryByLabelText("Shortcut")).toBeNull();
  });

  it("turning voice input on picks a provider the saved keys already serve", async () => {
    const { daemon } = mount([entry("audio.enabled", "false")]);
    fireEvent.click(await screen.findByRole("switch", { name: /Talk to Borg/ }));
    await waitFor(() => expect(saved(daemon, "audio.enabled")).toEqual(["true"]));
    // OpenAI is the one transcription provider with a key on file.
    expect(JSON.parse(saved(daemon, "audio.models")[0]!)).toEqual([{ provider: "openai" }]);
  });

  it("keeps the chain that is already there when turning voice input on", async () => {
    const chain = JSON.stringify([{ provider: "elevenlabs" }]);
    const { daemon } = mount([entry("audio.enabled", "false"), entry("audio.models", chain)]);
    fireEvent.click(await screen.findByRole("switch", { name: /Talk to Borg/ }));
    await waitFor(() => expect(saved(daemon, "audio.enabled")).toEqual(["true"]));
    expect(saved(daemon, "audio.models")).toEqual([]);
  });

  it("shows the everyday dictation choices and folds the rest under More options", async () => {
    const { daemon } = mount([
      entry("audio.enabled", "true"),
      entry("audio.models", JSON.stringify([{ provider: "openai" }])),
      entry("voice.hold_to_record", "true"),
      entry("voice.dictation_auto_send", "false"),
    ]);
    expect(await screen.findByLabelText("Language")).toBeTruthy();
    expect(screen.getByLabelText("Shortcut")).toBeTruthy();
    expect(screen.getByRole("switch", { name: /Hold to talk/ }).getAttribute("aria-checked")).toBe(
      "true",
    );
    expect(screen.getAllByRole("status")[0]!.textContent).toMatch(/Ready/);
    // Power-user knobs stay out of the way until asked for.
    expect(screen.queryByLabelText("Words Borg should know")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /More options/ }));
    const prompt = screen.getByLabelText("Words Borg should know");
    fireEvent.change(prompt, { target: { value: "Borg, Adelphi" } });
    fireEvent.blur(prompt);
    await waitFor(() => expect(saved(daemon, "audio.prompt")).toEqual(["Borg, Adelphi"]));
    const temperature = screen.getByLabelText("Temperature");
    fireEvent.change(temperature, { target: { value: "0.2" } });
    fireEvent.blur(temperature);
    await waitFor(() => expect(saved(daemon, "audio.temperature")).toEqual(["0.2"]));
    // Beam size and timestamps only mean something to the on-device decoder.
    expect(screen.queryByLabelText("Beam size")).toBeNull();
    expect(screen.queryByRole("switch", { name: /Timestamps/ })).toBeNull();
    fireEvent.click(screen.getByRole("switch", { name: /Space between Asian and Latin text/ }));
    await waitFor(() => expect(saved(daemon, "audio.cjk_spacing")).toEqual(["true"]));
    // Keeping recordings shows its window and folder only once it is on.
    expect(screen.queryByLabelText("Keep for (days)")).toBeNull();
    fireEvent.click(screen.getByRole("switch", { name: /Keep recordings/ }));
    await waitFor(() => expect(saved(daemon, "audio.keep_recordings")).toEqual(["true"]));
    fireEvent.click(screen.getByRole("switch", { name: /Send right away/ }));
    await waitFor(() => expect(saved(daemon, "voice.dictation_auto_send")).toEqual(["true"]));
  });

  it("kept recordings expose their window and folder", async () => {
    const { daemon } = mount([
      entry("audio.enabled", "true"),
      entry("audio.keep_recordings", "true"),
      entry("audio.keep_recordings_days", "30"),
    ]);
    fireEvent.click(await screen.findByRole("button", { name: /More options/ }));
    const days = await screen.findByLabelText("Keep for (days)");
    fireEvent.change(days, { target: { value: "7" } });
    fireEvent.blur(days);
    await waitFor(() => expect(saved(daemon, "audio.keep_recordings_days")).toEqual(["7"]));
    const folder = screen.getByLabelText("Folder");
    fireEvent.change(folder, { target: { value: " /Volumes/notes " } });
    fireEvent.blur(folder);
    await waitFor(() => expect(saved(daemon, "audio.recordings_dir")).toEqual(["/Volumes/notes"]));
  });

  it("shows the on-device decoder's knobs only for the local provider", async () => {
    const local = {
      ...CATALOG,
      transcription: [
        ...CATALOG.transcription,
        provider("local", "On this device (whisper.cpp)", {
          needsKey: false,
          hasKey: true,
          keyUrl: "",
        }),
      ],
    };
    const { daemon } = mount(
      [
        entry("audio.enabled", "true"),
        entry("audio.models", JSON.stringify([{ provider: "local" }])),
      ],
      local,
    );
    fireEvent.click(await screen.findByRole("button", { name: /More options/ }));
    expect(screen.getByLabelText("Beam size")).toBeTruthy();
    expect(screen.getByLabelText("Silence threshold")).toBeTruthy();
    fireEvent.click(screen.getByRole("switch", { name: /Timestamps/ }));
    await waitFor(() => expect(saved(daemon, "audio.timestamps")).toEqual(["true"]));
  });

  it("the background listener is a desktop-only option", async () => {
    const entries = [
      entry("audio.enabled", "true"),
      entry("audio.models", JSON.stringify([{ provider: "openai" }])),
    ];
    const web = renderApp(<VoiceSettingsPage />, {
      daemon: fakeDaemon({ GetSettings: { entries }, GetVoiceCatalog: CATALOG, SetSetting: {} }),
      kind: "web",
      wrapper: (children) => (
        <ThemeProvider>
          <SettingsProvider>{children}</SettingsProvider>
        </ThemeProvider>
      ),
    });
    fireEvent.click(await screen.findByRole("button", { name: /More options/ }));
    expect(screen.queryByRole("switch", { name: /Listen from the background/ })).toBeNull();
    web.unmount();
    mount(entries);
    fireEvent.click(await screen.findByRole("button", { name: /More options/ }));
    expect(screen.getByRole("switch", { name: /Listen from the background/ })).toBeTruthy();
  });

  it("records a key combination for the shortcut, and a lone modifier on its release", async () => {
    const { daemon } = mount([
      entry("audio.enabled", "true"),
      entry("audio.models", JSON.stringify([{ provider: "openai" }])),
      entry("voice.dictation_shortcut", "mod+shift+space"),
    ]);
    const field = (await screen.findByLabelText("Shortcut")) as HTMLInputElement;
    expect(field.value).toBe("Ctrl+Shift+Space");
    fireEvent.focus(field);
    expect(field.value).toBe("Listening…");
    fireEvent.keyDown(field, { key: "d", code: "KeyD", ctrlKey: true, altKey: true });
    await waitFor(() => expect(saved(daemon, "voice.dictation_shortcut")).toEqual(["mod+alt+d"]));

    fireEvent.focus(field);
    fireEvent.keyDown(field, { key: "Alt", code: "AltRight" });
    // Still held: nothing saved yet, a letter could still follow.
    expect(saved(daemon, "voice.dictation_shortcut")).toHaveLength(1);
    fireEvent.keyUp(field, { key: "Alt", code: "AltRight" });
    await waitFor(() =>
      expect(saved(daemon, "voice.dictation_shortcut")).toEqual(["mod+alt+d", "altright"]),
    );

    // A bare letter would swallow typing, so it is refused and Escape backs out.
    fireEvent.focus(field);
    fireEvent.keyDown(field, { key: "x", code: "KeyX" });
    expect(field.value).toBe("Listening…");
    fireEvent.keyDown(field, { key: "Escape", code: "Escape" });
    expect(field.value).toBe("Ctrl+Shift+Space");

    // A mouse side button is a trigger too; the buttons that click are not.
    fireEvent.focus(field);
    fireEvent.mouseDown(field, { button: 0 });
    expect(field.value).toBe("Listening…");
    fireEvent.mouseDown(field, { button: 4 });
    await waitFor(() => expect(saved(daemon, "voice.dictation_shortcut")).toHaveLength(3));
    expect(saved(daemon, "voice.dictation_shortcut")[2]).toBe("mouse4");

    fireEvent.click(screen.getByRole("button", { name: "Clear" }));
    await waitFor(() => expect(saved(daemon, "voice.dictation_shortcut")).toHaveLength(4));
    expect(saved(daemon, "voice.dictation_shortcut")[3]).toBe("");
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

  it("picking a provider voice turns daemon speech on; the device voice turns it off", async () => {
    const { daemon } = mount([entry("tts.enabled", "false")]);
    const voice = (await screen.findByLabelText("Voice")) as HTMLSelectElement;
    fireEvent.change(voice, { target: { value: "elevenlabs" } });
    await waitFor(() => expect(saved(daemon, "tts.enabled")).toEqual(["true"]));
    expect(JSON.parse(saved(daemon, "tts.models")[0]!)).toEqual([{ provider: "elevenlabs" }]);

    daemon.answer("GetSettings", {
      entries: [
        entry("tts.enabled", "true"),
        entry("tts.models", JSON.stringify([{ provider: "elevenlabs" }])),
      ],
    });
    fireEvent.change(voice, { target: { value: "" } });
    await waitFor(() => expect(saved(daemon, "tts.enabled")).toEqual(["true", "false"]));
  });

  it("turns spoken replies on through their setting", async () => {
    const { daemon } = mount([entry("voice.auto_tts", "false")]);
    fireEvent.click(await screen.findByRole("switch", { name: /Read replies aloud/ }));
    await waitFor(() => expect(saved(daemon, "voice.auto_tts")).toEqual(["true"]));
  });

  it("shows the provider's own default voice as the placeholder and saves an override", async () => {
    const { daemon } = mount([
      entry("tts.enabled", "true"),
      entry("tts.models", JSON.stringify([{ provider: "elevenlabs" }])),
    ]);
    const voice = (await screen.findByLabelText("Voice id")) as HTMLInputElement;
    expect(voice.placeholder).toBe("adam");
    fireEvent.change(voice, { target: { value: "rachel" } });
    fireEvent.blur(voice);
    await waitFor(() =>
      expect(saved(daemon, "tts.models")).toEqual([
        JSON.stringify([{ provider: "elevenlabs", voice: "rachel" }]),
      ]),
    );
    expect(screen.getAllByRole("status")[0]!.textContent).toMatch(/Not ready yet/);
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
    await screen.findByLabelText("Voice id");
    expect(screen.queryByLabelText(/API key/)).toBeNull();
  });

  it("phone shows only the fields the chosen provider reads, and why it is not ready", async () => {
    const { daemon } = mount(
      [entry("telephony.provider", "twilio"), entry("telephony.caller_id", "")],
      { ...CATALOG, phoneProblem: "Twilio needs your Account SID." },
    );
    expect(await screen.findByLabelText("Account SID")).toBeTruthy();
    expect(screen.getByLabelText("Twilio Auth Token")).toBeTruthy();
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
  it("saves once when Enter is followed by leaving the field", async () => {
    const { daemon } = mount([
      entry("tts.enabled", "true"),
      entry("tts.models", JSON.stringify([{ provider: "elevenlabs" }])),
    ]);
    const voice = await screen.findByLabelText("Voice id");
    fireEvent.change(voice, { target: { value: "rachel" } });
    fireEvent.keyDown(voice, { key: "Enter" });
    fireEvent.blur(voice);
    await waitFor(() => expect(saved(daemon, "tts.models")).toHaveLength(1));
  });

  it("keeps both edits when two fields save before the settings refresh", async () => {
    const { daemon } = mount([
      entry("tts.enabled", "true"),
      entry("tts.models", JSON.stringify([{ provider: "elevenlabs" }, { provider: "edge" }])),
    ]);
    const voice = await screen.findByLabelText("Voice id");
    fireEvent.click(screen.getByRole("button", { name: /More options/ }));
    const model = screen.getByLabelText("Model");
    fireEvent.change(voice, { target: { value: "rachel" } });
    fireEvent.blur(voice);
    fireEvent.change(model, { target: { value: "eleven_v3" } });
    fireEvent.blur(model);
    await waitFor(() => expect(saved(daemon, "tts.models")).toHaveLength(2));
    // The second write builds on the first, not on the stale snapshot.
    expect(JSON.parse(saved(daemon, "tts.models")[1]!)).toEqual([
      { provider: "elevenlabs", voice: "rachel", model: "eleven_v3" },
      { provider: "edge" },
    ]);
  });
  it("lets the same value be saved again after a save failed", async () => {
    const { daemon } = mount([entry("telephony.provider", "twilio")]);
    daemon.answer("SetSetting", new BorgError(GrpcStatus.Unavailable, "daemon down"));
    const sid = await screen.findByLabelText("Account SID");
    fireEvent.change(sid, { target: { value: "AC123" } });
    fireEvent.keyDown(sid, { key: "Enter" });
    await waitFor(() => expect(saved(daemon, "telephony.account_id")).toHaveLength(1));
    daemon.answer("SetSetting", {});
    await act(async () => {});
    fireEvent.keyDown(sid, { key: "Enter" });
    await waitFor(() => expect(saved(daemon, "telephony.account_id")).toEqual(["AC123", "AC123"]));
  });

  it("sends chain writes one at a time, each on top of the last", async () => {
    const { daemon } = mount([
      entry("tts.enabled", "true"),
      entry("tts.models", JSON.stringify([{ provider: "elevenlabs" }])),
    ]);
    let release: () => void = () => {};
    daemon.answer("SetSetting", () => new Promise((resolve) => (release = () => resolve({}))));
    const voice = await screen.findByLabelText("Voice id");
    fireEvent.click(screen.getByRole("button", { name: /More options/ }));
    const model = screen.getByLabelText("Model");
    fireEvent.change(voice, { target: { value: "rachel" } });
    fireEvent.blur(voice);
    fireEvent.change(model, { target: { value: "eleven_v3" } });
    fireEvent.blur(model);
    await waitFor(() => expect(saved(daemon, "tts.models")).toHaveLength(1));
    // The second write waits for the first.
    await act(async () => {});
    expect(saved(daemon, "tts.models")).toHaveLength(1);
    await act(async () => release());
    await waitFor(() => expect(saved(daemon, "tts.models")).toHaveLength(2));
    await act(async () => release());
    expect(JSON.parse(saved(daemon, "tts.models")[1]!)).toEqual([
      { provider: "elevenlabs", voice: "rachel", model: "eleven_v3" },
    ]);
  });
});
