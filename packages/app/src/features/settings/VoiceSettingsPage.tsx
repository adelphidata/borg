import { useEffect, useRef, useState } from "react";
import { chordOf, isMacPlatform } from "../../lib/keybinds";
import { QUICK_ENTRY_CHORD } from "../../lib/chords";
import { usePlatform } from "../../lib/platform";
import { hasDeviceVoice, useSpeaker, useSpeechSource } from "../../lib/speech";
import {
  readLocalVoiceChoice,
  readyProvider,
  useVoicePrefs,
  writeLocalVoiceChoice,
} from "../../lib/voice";
import { MODIFIER_CODES, parseShortcut, shortcutLabel } from "../conversation/dictation";
import {
  BackLink,
  DETAIL_COL,
  PageChrome,
  Pill,
  SECTION_LABEL,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useSettings } from "../../lib/settings";
import { useVoiceCatalog, type VoiceCatalog, type VoiceProvider } from "../../lib/voice";
import { withPrimaryProvider } from "./voiceChain";
import {
  CARD,
  FIELD,
  Field,
  KeyField,
  MoreOptions,
  ProviderSelect,
  SelectSetting,
  Status,
  Switch,
  TextSetting,
  useChain,
} from "./fields";
import { TranscriberPicker } from "./TranscriberPicker";

/** Languages a transcriber is commonly asked for; auto-detect is the default. */
const LANGUAGES: [string, string][] = [
  ["", "Detect automatically"],
  ["en", "English"],
  ["es", "Spanish"],
  ["fr", "French"],
  ["de", "German"],
  ["it", "Italian"],
  ["pt", "Portuguese"],
  ["nl", "Dutch"],
  ["pl", "Polish"],
  ["ru", "Russian"],
  ["uk", "Ukrainian"],
  ["tr", "Turkish"],
  ["ar", "Arabic"],
  ["he", "Hebrew"],
  ["hi", "Hindi"],
  ["ja", "Japanese"],
  ["ko", "Korean"],
  ["zh", "Chinese"],
];

/**
 * The dictation key. Shown as words; "Change" listens for the next press —
 * a chord with a modifier, or a single modifier held on its own.
 */
function ShortcutField({
  value,
  warning,
  onCommit,
}: {
  value: string;
  /** Shown when the key is set but cannot reach dictation. */
  warning?: string;
  onCommit: (v: string) => void;
}) {
  const isMac = isMacPlatform();
  const [listening, setListening] = useState(false);
  /** A modifier pressed alone, pending its release. */
  const candidate = useRef<string | null>(null);
  const label = shortcutLabel(value, isMac);
  const stop = () => {
    candidate.current = null;
    setListening(false);
  };
  const onKeyDown = (e: React.KeyboardEvent) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      stop();
      return;
    }
    const code = e.code.toLowerCase();
    if (code in MODIFIER_CODES) {
      // Alone so far; a letter after it makes a chord instead.
      if (!e.repeat) candidate.current = code;
      return;
    }
    candidate.current = null;
    const chord = chordOf(e.nativeEvent, isMac);
    if (parseShortcut(chord)) {
      onCommit(chord);
      stop();
    }
  };
  const onKeyUp = (e: React.KeyboardEvent) => {
    const code = e.code.toLowerCase();
    if (candidate.current === code) {
      onCommit(code);
      stop();
    }
  };
  const onMouseDown = (e: React.MouseEvent) => {
    // The first three buttons are for clicking; a side button can be the key.
    if (e.button < 3) return;
    e.preventDefault();
    onCommit(`mouse${e.button}`);
    stop();
  };
  return (
    <Field
      label="Shortcut"
      hint={
        listening ? (
          "Press a key combination, a modifier key alone, or a mouse side button. Esc cancels."
        ) : warning ? (
          <span className="text-alert">{warning}</span>
        ) : undefined
      }
    >
      {(id) => (
        <div className="flex items-center gap-2">
          <input
            id={id}
            readOnly
            className={`${FIELD} cursor-default`}
            value={listening ? "Listening…" : label || "None"}
            onFocus={() => setListening(true)}
            onBlur={stop}
            onKeyDown={listening ? onKeyDown : undefined}
            onKeyUp={listening ? onKeyUp : undefined}
            onMouseDown={listening ? onMouseDown : undefined}
          />
          {value !== "" && !listening && (
            <Pill square onClick={() => onCommit("")}>
              Clear
            </Pill>
          )}
        </div>
      )}
    </Field>
  );
}

/** Which microphone this device records with; a choice kept on this device. */
function MicrophoneField() {
  const [devices, setDevices] = useState<MediaDeviceInfo[]>();
  const [chosen, setChosen] = useState(() => readLocalVoiceChoice("microphone"));
  useEffect(() => {
    const media = navigator.mediaDevices;
    if (!media || typeof media.enumerateDevices !== "function") return;
    let live = true;
    const refresh = () =>
      void media
        .enumerateDevices()
        .then((all) => live && setDevices(all.filter((d) => d.kind === "audioinput")))
        .catch(() => live && setDevices([]));
    refresh();
    media.addEventListener?.("devicechange", refresh);
    return () => {
      live = false;
      media.removeEventListener?.("devicechange", refresh);
    };
  }, []);
  if (devices === undefined) return null;
  const named = devices.filter((d) => d.deviceId !== "");
  const options: [string, string][] = [
    ["", "System default"],
    ...named.map((d, i): [string, string] => [d.deviceId, d.label || `Microphone ${i + 1}`]),
  ];
  return (
    <SelectSetting
      label="Microphone"
      value={chosen}
      options={options}
      onPick={(v) => {
        setChosen(v);
        writeLocalVoiceChoice("microphone", v);
      }}
    />
  );
}

/** Which of this device's own voices reads replies; kept on this device. */
function DeviceVoiceField() {
  const [voices, setVoices] = useState<SpeechSynthesisVoice[]>([]);
  const [chosen, setChosen] = useState(() => readLocalVoiceChoice("builtInVoice"));
  useEffect(() => {
    if (!hasDeviceVoice()) return;
    const synth = globalThis.speechSynthesis;
    const refresh = () => setVoices(synth.getVoices());
    refresh();
    synth.addEventListener?.("voiceschanged", refresh);
    return () => synth.removeEventListener?.("voiceschanged", refresh);
  }, []);
  if (voices.length === 0) return null;
  const options: [string, string][] = [
    ["", "Default"],
    ...voices.map((v): [string, string] => [v.name, `${v.name} (${v.lang})`]),
  ];
  return (
    <SelectSetting
      label="Which voice"
      value={chosen}
      options={options}
      onPick={(v) => {
        setChosen(v);
        writeLocalVoiceChoice("builtInVoice", v);
      }}
    />
  );
}

/** Voice input: the microphone, its key, and what happens to the words. */
function DictationSection({
  providers,
  ready,
  refetch,
}: {
  providers: VoiceProvider[];
  ready: boolean;
  refetch: () => void;
}) {
  const { settings, save } = useSettings();
  const { kind } = usePlatform();
  const prefs = useVoicePrefs();
  const chain = useChain("audio.models");
  const { row, writeChain, setField } = chain;
  if (settings.state !== "ready") return null;
  const enabled = settings.data.get("audio.enabled")?.value === "true";
  const provider = providers.find((p) => p.id === row?.provider);
  const value = (key: string) => settings.data.get(key)?.value ?? "";
  const turnOn = async (on: boolean) => {
    // Turning on with no provider yet: a saved chat key that can transcribe
    // is used without a second step, which is what "on" should mean.
    if (on && !row) {
      const pick = readyProvider(providers);
      if (pick) await writeChain((json) => withPrimaryProvider(json, pick.id));
    }
    await save("audio.enabled", String(on));
  };
  const shortcut = parseShortcut(prefs.shortcut);

  return (
    <section>
      <div className={SECTION_LABEL}>Voice input</div>
      <div className={CARD}>
        <Switch label="Talk to Borg" on={enabled} onChange={(on) => void turnOn(on)} />
        {enabled && (
          <>
            <TranscriberPicker
              chain={chain}
              providers={providers}
              ready={ready}
              refetch={refetch}
            />
            <SelectSetting
              label="Language"
              value={prefs.language}
              options={LANGUAGES}
              onPick={(v) => void save("audio.language", v)}
            />
            <ShortcutField
              value={prefs.shortcut}
              warning={
                kind === "desktop" &&
                shortcut?.kind === "chord" &&
                shortcut.chord === QUICK_ENTRY_CHORD
                  ? "Quick entry already uses this key."
                  : undefined
              }
              onCommit={(v) => void save("voice.dictation_shortcut", v)}
            />
            <Switch
              label="Hold to talk"
              on={prefs.holdToRecord}
              onChange={(on) => void save("voice.hold_to_record", String(on))}
            />
            <Switch
              label="Send right away"
              on={prefs.autoSend}
              onChange={(on) => void save("voice.dictation_auto_send", String(on))}
            />
            <MicrophoneField />
            <MoreOptions>
              {provider && provider.defaultModel !== "" && (
                <TextSetting
                  label="Model"
                  value={row?.model ?? ""}
                  placeholder={provider.defaultModel}
                  onCommit={(v) => setField("model", v)}
                />
              )}
              <TextSetting
                label="Words Borg should know"
                value={value("audio.prompt")}
                placeholder="Borg, Adelphi, Kubernetes"
                onCommit={(v) => save("audio.prompt", v)}
              />
              <TextSetting
                label="Temperature"
                value={value("audio.temperature")}
                placeholder="0"
                onCommit={(v) => save("audio.temperature", v.trim() === "" ? "0" : v.trim())}
              />
              {row?.provider === "local" && (
                <>
                  <TextSetting
                    label="Beam size"
                    value={value("audio.beam_size")}
                    placeholder="5"
                    onCommit={(v) => save("audio.beam_size", v.trim() === "" ? "5" : v.trim())}
                  />
                  <TextSetting
                    label="Silence threshold"
                    value={value("audio.no_speech_threshold")}
                    placeholder="0.6"
                    onCommit={(v) =>
                      save("audio.no_speech_threshold", v.trim() === "" ? "0.6" : v.trim())
                    }
                  />
                  <Switch
                    label="Timestamps"
                    on={value("audio.timestamps") === "true"}
                    onChange={(on) => void save("audio.timestamps", String(on))}
                  />
                </>
              )}
              <Switch
                label="Sounds"
                on={prefs.beep}
                onChange={(on) => void save("voice.beep_enabled", String(on))}
              />
              <Switch
                label="Copy to clipboard"
                on={prefs.copyTranscript}
                onChange={(on) => void save("voice.copy_transcript", String(on))}
              />
              <Switch
                label="Keep recordings"
                on={value("audio.keep_recordings") === "true"}
                onChange={(on) => void save("audio.keep_recordings", String(on))}
              />
              {value("audio.keep_recordings") === "true" && (
                <>
                  <TextSetting
                    label="Keep for (days)"
                    value={value("audio.keep_recordings_days")}
                    placeholder="30"
                    onCommit={(v) =>
                      save("audio.keep_recordings_days", v.trim() === "" ? "30" : v.trim())
                    }
                  />
                  <TextSetting
                    label="Folder"
                    value={value("audio.recordings_dir")}
                    placeholder="~/.borg/recordings"
                    onCommit={(v) => save("audio.recordings_dir", v.trim())}
                  />
                </>
              )}
              <Switch
                label="Space between Asian and Latin text"
                on={value("audio.cjk_spacing") === "true"}
                onChange={(on) => void save("audio.cjk_spacing", String(on))}
              />
              {kind === "desktop" && (
                <Switch
                  label="Listen from the background"
                  warning={
                    shortcut?.kind === "chord" ? undefined : "Needs a shortcut with a modifier key."
                  }
                  on={prefs.backgroundShortcut}
                  onChange={(on) => void save("voice.background_shortcut", String(on))}
                />
              )}
            </MoreOptions>
          </>
        )}
      </div>
    </section>
  );
}

/** The value the voice picker shows for the device's own voice. */
const DEVICE_VOICE = "";

/** Spoken replies: who speaks, and whether every reply is read aloud. */
function SpeechSection({
  providers,
  ready,
  refetch,
}: {
  providers: VoiceProvider[];
  ready: boolean;
  refetch: () => void;
}) {
  const { settings, save } = useSettings();
  const prefs = useVoicePrefs();
  const { row, writeChain, setField } = useChain("tts.models");
  const [problem, setProblem] = useState<string>();
  const speaker = useSpeaker(setProblem);
  if (settings.state !== "ready") return null;
  const viaProvider = settings.data.get("tts.enabled")?.value === "true";
  const provider = viaProvider ? providers.find((p) => p.id === row?.provider) : undefined;
  const device = hasDeviceVoice();
  const pick = async (id: string) => {
    if (id === DEVICE_VOICE) {
      await save("tts.enabled", "false");
      return;
    }
    await writeChain((json) => withPrimaryProvider(json, id));
    if (!viaProvider) await save("tts.enabled", "true");
  };
  const sample = () => {
    setProblem(undefined);
    speaker.speak("settings-sample", "Hi, this is how I sound.");
  };
  const speaking = speaker.state?.id === "settings-sample";

  return (
    <section>
      <div className={SECTION_LABEL}>Spoken replies</div>
      <div className={CARD}>
        <Switch
          label="Read replies aloud"
          on={prefs.autoSpeak}
          onChange={(on) => void save("voice.auto_tts", String(on))}
        />
        <Field
          label="Voice"
          hint={!viaProvider && !device ? "This device has no built-in voice." : undefined}
        >
          {(id) => (
            <select
              id={id}
              className={FIELD}
              value={viaProvider ? (row?.provider ?? "") : DEVICE_VOICE}
              onChange={(e) => void pick(e.target.value)}
            >
              <option value={DEVICE_VOICE}>This device's voice</option>
              {viaProvider && row && !providers.some((p) => p.id === row.provider) && (
                <option value={row.provider}>{row.provider}</option>
              )}
              {providers.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.label}
                </option>
              ))}
            </select>
          )}
        </Field>
        {!viaProvider && <DeviceVoiceField />}
        {provider && <KeyField key={provider.id} provider={provider} onSaved={refetch} />}
        {provider && !provider.requiresCommand && (
          <TextSetting
            label="Voice id"
            value={row?.voice ?? ""}
            placeholder={provider.defaultVoice || "Provider default"}
            onCommit={(v) => setField("voice", v)}
          />
        )}
        {provider?.requiresCommand && (
          <TextSetting
            label="Command"
            hint="Read {text_file}, write audio to {output_path}."
            value={row?.command ?? ""}
            onCommit={(v) => setField("command", v)}
          />
        )}
        <div className="flex items-center gap-3">
          <Status
            ok={viaProvider ? ready : device}
            okText={viaProvider ? "Ready" : "Ready, using this device"}
            problem={
              viaProvider
                ? provider
                  ? "Not ready yet: add the missing key or command above."
                  : "Choose a provider to finish."
                : "No voice available on this device."
            }
          />
          <span className="flex-1" />
          {speaker.canSpeak && (
            <Pill square onClick={sample} aria-pressed={speaking}>
              {speaking ? "Stop" : "Play a sample"}
            </Pill>
          )}
        </div>
        {problem && <div className="text-[12px] leading-[1.45] text-alert">{problem}</div>}
        {provider && provider.defaultModel !== "" && (
          <MoreOptions>
            <TextSetting
              label="Model"
              value={row?.model ?? ""}
              placeholder={provider.defaultModel}
              onCommit={(v) => setField("model", v)}
            />
          </MoreOptions>
        )}
      </div>
    </section>
  );
}

function PhoneSection({ catalog, refetch }: { catalog: VoiceCatalog; refetch: () => void }) {
  const { settings, save } = useSettings();
  if (settings.state !== "ready") return null;
  const value = (key: string) => settings.data.get(key)?.value ?? "";
  const chosen = value("telephony.provider");
  const phone = catalog.phone.find((p) => p.provider?.id === chosen);
  const provider = phone?.provider;

  return (
    <section>
      <div className={SECTION_LABEL}>Phone calls</div>
      <div className={CARD}>
        <ProviderSelect
          label="Provider"
          providers={catalog.phone.flatMap((p) => (p.provider ? [p.provider] : []))}
          value={chosen}
          offLabel="Off"
          onPick={(id) => void save("telephony.provider", id)}
        />
        {phone && provider && (
          <>
            <KeyField key={provider.id} provider={provider} onSaved={refetch} />
            {phone.usesAccountId && (
              <TextSetting
                label="Account SID"
                value={value("telephony.account_id")}
                placeholder="AC…"
                onCommit={(v) => save("telephony.account_id", v)}
              />
            )}
            <TextSetting
              label={phone.callerIdLabel}
              value={value("telephony.caller_id")}
              onCommit={(v) => save("telephony.caller_id", v)}
            />
            {phone.usesAgentId && (
              <TextSetting
                label={phone.agentRequired ? "Agent id" : "Agent id (optional)"}
                hint="The agent's prompt gets the task as {{task}}."
                value={value("telephony.agent_id")}
                onCommit={(v) => save("telephony.agent_id", v)}
              />
            )}
            <TextSetting
              label="Numbers Borg may call"
              hint="Comma-separated, with country code, like +1415."
              value={value("telephony.allowed_numbers")}
              placeholder="Any number"
              onCommit={(v) => save("telephony.allowed_numbers", v)}
            />
            <Status
              ok={catalog.phoneProblem === ""}
              okText="Ready to call"
              problem={catalog.phoneProblem}
            />
          </>
        )}
      </div>
    </section>
  );
}

/**
 * Settings → Advanced → Voice & phone.
 *
 * Three outcomes, each one switch deep: Borg listens, Borg speaks, Borg
 * calls. Every provider list comes from the daemon's registry, so a provider
 * added there appears here with no change to the app. What a first-time user
 * never needs sits under "More options"; the fallback chain, timeouts and
 * voice-loop tuning stay in the raw Advanced list.
 */
export function VoiceSettingsPage() {
  const isMobile = useIsMobile();
  const { saveError } = useSettings();
  const catalog = useVoiceCatalog();
  useSpeechSource();

  return (
    <PageChrome title="Voice & phone">
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className={DETAIL_COL}>
          <BackLink to="/settings" label="Settings" />
          <div>
            <h2 className="m-0 text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
              Voice & phone
            </h2>
          </div>
          {saveError && (
            <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
              {saveError.message}
            </div>
          )}
          <DaemonState query={catalog} label="voice providers">
            {(data) => (
              <>
                <DictationSection
                  providers={data.transcription}
                  ready={data.transcriptionReady}
                  refetch={catalog.refetch}
                />
                <SpeechSection
                  providers={data.speech}
                  ready={data.speechReady}
                  refetch={catalog.refetch}
                />
                <PhoneSection catalog={data} refetch={catalog.refetch} />
              </>
            )}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
