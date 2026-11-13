import { describe, expect, it } from "vitest";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { ThemeProvider } from "@borg/ui";
import { SettingsProvider } from "../../lib/settings";
import { fakeDaemon, renderApp, type UnaryTable } from "../../test/harness";
import { entry, status, voiceCatalog, voiceProvider } from "../../test/meetings";
import { MeetingsSettingsPage, calendarLabel } from "./MeetingsSettingsPage";

const OPENAI = voiceProvider("openai", "OpenAI", { hasKey: true });
const DEEPGRAM = voiceProvider("deepgram", "Deepgram");

function mount(entries: ReturnType<typeof entry>[], table: UnaryTable = {}) {
  const daemon = fakeDaemon({
    GetSettings: { entries },
    GetVoiceCatalog: voiceCatalog([OPENAI, DEEPGRAM]),
    GetMeetingsStatus: status(),
    SetSetting: {},
    SetApiKey: {},
    ...table,
  });
  const view = renderApp(<MeetingsSettingsPage />, {
    daemon,
    wrapper: (children) => (
      <ThemeProvider>
        <SettingsProvider>{children}</SettingsProvider>
      </ThemeProvider>
    ),
  });
  return { ...view, daemon };
}

const writes = (daemon: ReturnType<typeof fakeDaemon>) =>
  daemon.calls
    .filter((c) => c.method === "SetSetting")
    .map((c) => c.request as { key: string; value: string });

const ON = [entry("meetings.enabled", "true"), entry("audio.models", '[{"provider":"openai"}]')];

describe("MeetingsSettingsPage", () => {
  it("is one switch while off", async () => {
    mount([]);
    const toggle = await screen.findByRole("switch", { name: "Take notes in meetings" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    expect(screen.queryByLabelText("Provider")).toBeNull();
  });

  it("turns on with the transcriber a key is already saved for", async () => {
    const { daemon } = mount([]);
    fireEvent.click(await screen.findByRole("switch", { name: "Take notes in meetings" }));
    await waitFor(() =>
      expect(writes(daemon)).toEqual([
        { key: "audio.models", value: JSON.stringify([{ provider: "openai" }]) },
        { key: "meetings.enabled", value: "true" },
      ]),
    );
  });

  it("keeps the transcriber already chosen when turned on", async () => {
    const { daemon } = mount([entry("audio.models", '[{"provider":"deepgram"}]')]);
    fireEvent.click(await screen.findByRole("switch", { name: "Take notes in meetings" }));
    await waitFor(() =>
      expect(writes(daemon)).toEqual([{ key: "meetings.enabled", value: "true" }]),
    );
  });

  it("turns off with one write", async () => {
    const { daemon } = mount(ON);
    fireEvent.click(await screen.findByRole("switch", { name: "Take notes in meetings" }));
    await waitFor(() =>
      expect(writes(daemon)).toEqual([{ key: "meetings.enabled", value: "false" }]),
    );
  });

  it("offers the transcribers the daemon serves, and only those", async () => {
    mount(ON, {
      GetVoiceCatalog: voiceCatalog([OPENAI, voiceProvider("mistral", "Mistral Voxtral")]),
    });
    const select = (await screen.findByLabelText("Provider")) as HTMLSelectElement;
    const offered = within(select)
      .getAllByRole("option")
      .map((o) => o.textContent);
    expect(offered).toEqual(["OpenAI", "Mistral Voxtral"]);
  });

  it("says Ready when the daemon can transcribe, and what is missing when it cannot", async () => {
    const ready = mount(ON);
    expect(await screen.findByText("Ready")).toBeTruthy();
    ready.unmount();
    mount(ON, { GetMeetingsStatus: status({ transcriptionReady: false }) });
    expect(
      await screen.findByText("Not ready yet: add the missing key or command above."),
    ).toBeTruthy();
  });

  it("asks for a provider when none is chosen", async () => {
    mount([entry("meetings.enabled", "true")], {
      GetMeetingsStatus: status({ transcriptionReady: false }),
    });
    expect(await screen.findByText("Choose a provider to finish.")).toBeTruthy();
  });

  it("names the connected calendar and links to it", async () => {
    mount(ON, {
      GetMeetingsStatus: status({
        calendar: { connected: true, kind: "google", accountLabel: "me@example.com" },
      }),
    });
    expect(await screen.findByText("Google · me@example.com")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Change" }).getAttribute("href")).toBe(
      "/connections/calendar",
    );
  });

  it("explains that only a Mac notices meetings by itself", async () => {
    mount(ON, { GetMeetingsStatus: status({ detectionSupported: false }) });
    expect(await screen.findByText(/notices meetings automatically on a Mac/)).toBeTruthy();
  });

  it("keeps the folder and the recording rules under More options", async () => {
    const { daemon } = mount(ON);
    fireEvent.click(await screen.findByRole("button", { name: /More options/ }));
    const folder = screen.getByLabelText("Folder") as HTMLInputElement;
    expect(folder.placeholder).toBe("~/.borg/recordings/meetings");
    fireEvent.change(folder, { target: { value: "~/Notes/meetings" } });
    fireEvent.blur(folder);
    fireEvent.click(screen.getByRole("switch", { name: "Keep audio" }));
    fireEvent.click(screen.getByRole("switch", { name: /Ask before recording/ }));
    await waitFor(() =>
      expect(writes(daemon)).toEqual([
        { key: "meetings.dir", value: "~/Notes/meetings" },
        { key: "meetings.keep_audio", value: "true" },
        { key: "meetings.ask_before_recording", value: "false" },
      ]),
    );
  });

  it("warns what turning off the question means", async () => {
    mount([...ON, entry("meetings.ask_before_recording", "false")]);
    fireEvent.click(await screen.findByRole("button", { name: /More options/ }));
    expect(screen.getByText(/start recording as soon as they are noticed/)).toBeTruthy();
  });
});

describe("calendarLabel", () => {
  it.each([
    [{ connected: false, kind: "", accountLabel: "" }, "Not connected"],
    [{ connected: true, kind: "google", accountLabel: "a@b.co" }, "Google · a@b.co"],
    [
      { connected: true, kind: "ics", accountLabel: "calendar.example.com" },
      "ICS feed · calendar.example.com",
    ],
    [{ connected: true, kind: "", accountLabel: "" }, "Connected"],
    [undefined, "Not connected"],
  ])("%j reads %s", (calendar, label) => {
    expect(calendarLabel(calendar)).toBe(label);
  });
});
