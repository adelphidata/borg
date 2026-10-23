import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { CheckIcon, CloseIcon, MicIcon, Pill } from "@borg/ui";
import { acceleratorForChord } from "../../lib/chords";
import { isMacPlatform } from "../../lib/keybinds";
import { useNative } from "../../lib/platform";
import { useSettings } from "../../lib/settings";
import {
  readLocalVoiceChoice,
  useVoiceCatalog,
  useVoicePrefs,
  type VoiceProvider,
} from "../../lib/voice";
import { formatRecordingClock, parseShortcut, shortcutLabel } from "./dictation";
import { canRecord, useDictation } from "./useDictation";
import { withPrimaryProvider } from "../settings/voiceChain";

/** Transcription providers in the order a one-click setup prefers them. */
const PREFERRED = ["openai", "groq", "deepgram", "elevenlabs", "mistral", "xai"];

/**
 * The provider a key is already saved for, best first; undefined when none.
 * Keyless providers do not count: "no key needed" says nothing about a model
 * file or a build flag, so one press on them could land on "not ready".
 */
export function readyProvider(providers: VoiceProvider[]): VoiceProvider | undefined {
  const keyed = providers.filter((p) => p.needsKey && p.hasKey && !p.requiresCommand);
  return (
    PREFERRED.map((id) => keyed.find((p) => p.id === id)).find((p) => p !== undefined) ?? keyed[0]
  );
}

/**
 * The composer's microphone, the way a chat app has one: always there, one
 * click to talk.
 *
 * When transcription is not set up yet the click opens a small card instead
 * of recording. If a chat key already serves a transcription provider the
 * card turns voice input on with that provider in one press; otherwise it
 * points at Settings. A microphone that is there but silent teaches more
 * than one that is missing.
 */
export function VoiceInputButton({
  onText,
  onError,
}: {
  /**
   * The recognized words, trimmed and non-empty. `send` is true when Settings
   * says a dictated message goes straight out rather than into the box.
   */
  onText: (text: string, options: { send: boolean }) => void;
  /** A plain-words failure; `undefined` clears an earlier one. */
  onError: (message: string | undefined) => void;
}) {
  const catalog = useVoiceCatalog();
  const prefs = useVoicePrefs();
  const native = useNative();
  // A failed refresh keeps the last answer: a network blip must not cut off a recording.
  const lastReady = useRef(false);
  if (catalog.state === "ready") lastReady.current = catalog.data.transcriptionReady;
  const ready = lastReady.current;
  const [setupOpen, setSetupOpen] = useState(false);
  const [deviceId, setDeviceId] = useState(() => readLocalVoiceChoice("microphone"));
  // Settings writes the choice from another page; pick it up on the next recording.
  useEffect(() => {
    const sync = () => setDeviceId(readLocalVoiceChoice("microphone"));
    window.addEventListener("storage", sync);
    window.addEventListener("focus", sync);
    return () => {
      window.removeEventListener("storage", sync);
      window.removeEventListener("focus", sync);
    };
  }, []);

  const dictation = useDictation({
    enabled: ready,
    shortcut: prefs.shortcut,
    holdToRecord: prefs.holdToRecord,
    beep: prefs.beep,
    deviceId,
    language: prefs.language,
    onText: (text) => {
      if (prefs.copyTranscript) void navigator.clipboard?.writeText(text).catch(() => {});
      onText(text, { send: prefs.autoSend });
    },
    onError,
  });

  // The desktop shell can listen from behind another app; the chord is sent
  // up as an accelerator, or cleared. A lone modifier cannot be one.
  const shortcut = parseShortcut(prefs.shortcut);
  const background =
    ready && prefs.backgroundShortcut && shortcut?.kind === "chord"
      ? acceleratorForChord(shortcut.chord)
      : "";
  useEffect(() => {
    native.setDictationHotkey(background);
    return () => native.setDictationHotkey("");
  }, [native, background]);

  useEffect(() => {
    if (ready) setSetupOpen(false);
  }, [ready]);

  if (!canRecord()) return null;

  const { phase } = dictation;
  const keyHint = shortcutLabel(prefs.shortcut, isMacPlatform());
  const hint =
    phase === "recording"
      ? "Done (click, press the shortcut, or let it go)"
      : phase === "transcribing"
        ? "Transcribing…"
        : ready
          ? keyHint
            ? `Speak instead of typing (${keyHint})`
            : "Speak instead of typing"
          : "Set up voice input";

  return (
    <>
      {phase !== "idle" && (
        <RecordingBar
          phase={phase}
          seconds={dictation.seconds}
          level={dictation.level}
          onCancel={dictation.cancel}
        />
      )}
      {setupOpen && !ready && catalog.state === "ready" && (
        <SetupCard providers={catalog.data.transcription} onClose={() => setSetupOpen(false)} />
      )}
      <button
        type="button"
        // One stable name; the state rides on pressed/busy, as screen readers expect.
        aria-label="Voice input"
        title={hint}
        aria-pressed={phase === "recording"}
        aria-busy={phase === "transcribing"}
        disabled={phase === "transcribing"}
        onClick={() => {
          if (!ready) setSetupOpen((v) => !v);
          else dictation.toggle();
        }}
        className={`relative z-10 flex h-[29px] w-[29px] flex-none cursor-pointer items-center justify-center rounded-[9px] border-0 disabled:cursor-wait ${
          phase === "recording" ? "bg-neon text-ink" : "bg-transparent text-ink3 hover:text-ink2"
        } ${phase === "transcribing" ? "opacity-50" : ""}`}
      >
        {/* While recording this is the Done button: the one place the eye
            already is, and the one control the hand is already on. */}
        {phase === "recording" ? (
          <CheckIcon size={15} stroke="currentColor" />
        ) : (
          <MicIcon size={15} />
        )}
      </button>
    </>
  );
}

/** How many bars the level meter draws. */
const BARS = 14;

/**
 * Covers the box while the microphone is on: a clock, a live level, and the
 * way out that drops the audio (also Escape). Finishing is the mic button
 * itself, which turns into a check for the duration.
 */
function RecordingBar({
  phase,
  seconds,
  level,
  onCancel,
}: {
  phase: "recording" | "transcribing";
  seconds: number;
  level: number;
  onCancel: () => void;
}) {
  const recording = phase === "recording";
  return (
    <div
      role="status"
      aria-label={recording ? "Recording" : "Transcribing"}
      className="absolute inset-0 z-[5] flex items-center gap-3 rounded-[inherit] bg-surface2 pr-[46px] pl-4"
    >
      <button
        type="button"
        aria-label="Cancel recording"
        title="Cancel (Esc)"
        onClick={onCancel}
        disabled={!recording}
        className="flex h-[26px] w-[26px] flex-none cursor-pointer items-center justify-center rounded-full border-0 bg-field text-ink2 hover:text-ink disabled:cursor-wait disabled:opacity-50"
      >
        <CloseIcon size={13} />
      </button>
      <div
        className="flex h-6 min-w-0 flex-1 items-center justify-center gap-[3px]"
        aria-hidden="true"
      >
        {Array.from({ length: BARS }, (_, i) => {
          // Bars nearer the middle stand taller, so the meter reads as a
          // waveform rather than a row of equal ticks.
          const shape = 1 - Math.abs(i - (BARS - 1) / 2) / ((BARS + 1) / 2);
          const h = recording ? 3 + Math.round(level * 18 * (0.4 + 0.6 * shape)) : 3;
          return (
            <span
              key={i}
              className={`w-[3px] rounded-full transition-[height] duration-75 ${
                recording ? "bg-alert" : "bg-ink3"
              }`}
              style={{ height: `${h}px` }}
            />
          );
        })}
      </div>
      <span className="flex-none text-[12px] leading-none text-ink2 tabular-nums">
        {recording ? formatRecordingClock(seconds) : "Transcribing…"}
      </span>
    </div>
  );
}

/**
 * What the microphone shows before voice input is set up. One press when a
 * saved chat key can do the job; otherwise the way to Settings.
 */
function SetupCard({ providers, onClose }: { providers: VoiceProvider[]; onClose: () => void }) {
  const navigate = useNavigate();
  const { settings, save, saving } = useSettings();
  const provider = readyProvider(providers);
  const [failed, setFailed] = useState(false);
  const enable = async () => {
    if (!provider || settings.state !== "ready") return;
    setFailed(false);
    const chain = settings.data.get("audio.models")?.value;
    const ok =
      (await save("audio.models", withPrimaryProvider(chain, provider.id))) &&
      (await save("audio.enabled", "true"));
    if (!ok) setFailed(true);
  };
  return (
    <div
      role="dialog"
      aria-label="Set up voice input"
      className="absolute right-0 bottom-[calc(100%+8px)] z-20 w-[300px] rounded-[12px] border border-line2 bg-surface2 p-4 shadow-(--plateGlow)"
    >
      <div className="text-[13.5px] leading-[1.3] font-medium text-ink">Talk instead of typing</div>
      <div className="mt-1.5 text-[12px] leading-[1.45] text-ink3">
        {provider
          ? `Your ${provider.label} key can turn speech into text. Turn it on?`
          : "Borg needs a transcription provider to understand your voice. Pick one in Settings."}
      </div>
      {failed && (
        <div className="mt-2 text-[12px] leading-[1.45] text-alert">
          That didn't save. Try again, or set it up in Settings.
        </div>
      )}
      <div className="mt-3 flex items-center justify-end gap-2">
        <Pill square onClick={onClose}>
          Not now
        </Pill>
        {provider ? (
          <Pill variant="primary" square onClick={() => void enable()}>
            {saving ? "Turning on…" : `Use ${provider.label}`}
          </Pill>
        ) : (
          <Pill variant="primary" square onClick={() => navigate("/settings/voice")}>
            Open Settings
          </Pill>
        )}
      </div>
    </div>
  );
}
