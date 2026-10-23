import type { voice } from "@borg/client";
import type { DaemonSetting } from "./settings";
import { useMemo } from "react";
import { useSettings } from "./settings";
import { useQuery, type QueryResult } from "./useDaemon";

/**
 * The daemon's voice and phone providers, and which are ready right now —
 * `Voice.GetVoiceCatalog`.
 *
 * Every surface that names a speech, transcription or phone provider reads
 * this; the app carries no provider list of its own. It re-asks whenever a
 * voice-related setting changes, so turning transcription on in Settings
 * brings the composer's microphone up without a reload.
 */
export type VoiceCatalog = voice.VoiceCatalog;
export type VoiceProvider = voice.VoiceProvider;
export type PhoneProvider = voice.PhoneProvider;

/** Settings prefixes whose change can change what the catalog reports. */
const VOICE_PREFIXES = ["tts.", "audio.", "telephony."];

/** A fingerprint of the voice settings, used as the catalog's refetch key. */
export function voiceSettingsKey(settings: Map<string, DaemonSetting>): string {
  return [...settings.values()]
    .filter((s) => VOICE_PREFIXES.some((p) => s.key.startsWith(p)))
    .map((s) => `${s.key}=${s.value}`)
    .sort()
    .join("\n");
}

export function useVoiceCatalog(): QueryResult<VoiceCatalog> {
  const { settings } = useSettings();
  // The composer re-renders per keystroke; fingerprint once per settings snapshot.
  const data = settings.state === "ready" ? settings.data : undefined;
  const key = useMemo(() => (data ? voiceSettingsKey(data) : ""), [data]);
  return useQuery((borg) => borg.voice.getVoiceCatalog({}), [key]);
}

/** Transcription providers in the order a one-click setup prefers them. */
const PREFERRED_TRANSCRIBERS = ["openai", "groq", "deepgram", "elevenlabs", "mistral", "xai"];

/**
 * The provider a key is already saved for, best first; undefined when none.
 * Keyless providers do not count: "no key needed" says nothing about a model
 * file or a build flag, so one press on them could land on "not ready".
 * Shared by the composer's setup card and the Settings switch, so both turn
 * voice input on with the same provider.
 */
export function readyProvider(providers: VoiceProvider[]): VoiceProvider | undefined {
  const keyed = providers.filter((p) => p.needsKey && p.hasKey && !p.requiresCommand);
  return (
    PREFERRED_TRANSCRIBERS.map((id) => keyed.find((p) => p.id === id)).find(
      (p) => p !== undefined,
    ) ?? keyed[0]
  );
}

/** The daemon's compiled defaults, repeated here for the moment before the snapshot lands. */
export const VOICE_DEFAULTS = {
  shortcut: "mod+shift+space",
  holdToRecord: true,
  autoSend: false,
  copyTranscript: false,
  beep: true,
  autoSpeak: false,
  backgroundShortcut: false,
  language: "",
} as const;

/** How dictation and spoken replies behave, as Settings has them. */
export interface VoicePrefs {
  /** `voice.dictation_shortcut`: a chord or a lone modifier code. */
  shortcut: string;
  holdToRecord: boolean;
  autoSend: boolean;
  copyTranscript: boolean;
  beep: boolean;
  /** `voice.auto_tts`: read each reply aloud as it finishes. */
  autoSpeak: boolean;
  /** Desktop only: the shortcut also works while Borg is behind another app. */
  backgroundShortcut: boolean;
  /** `audio.language`: BCP-47, empty for auto-detect. */
  language: string;
}

/** Reads the dictation preferences out of a settings snapshot. */
export function voicePrefsOf(settings: Map<string, DaemonSetting> | undefined): VoicePrefs {
  const get = (key: string) => settings?.get(key)?.value;
  const bool = (key: string, fallback: boolean) => {
    const v = get(key);
    return v === undefined ? fallback : v === "true";
  };
  return {
    shortcut: get("voice.dictation_shortcut") || VOICE_DEFAULTS.shortcut,
    holdToRecord: bool("voice.hold_to_record", VOICE_DEFAULTS.holdToRecord),
    autoSend: bool("voice.dictation_auto_send", VOICE_DEFAULTS.autoSend),
    copyTranscript: bool("voice.copy_transcript", VOICE_DEFAULTS.copyTranscript),
    beep: bool("voice.beep_enabled", VOICE_DEFAULTS.beep),
    autoSpeak: bool("voice.auto_tts", VOICE_DEFAULTS.autoSpeak),
    backgroundShortcut: bool("voice.background_shortcut", VOICE_DEFAULTS.backgroundShortcut),
    language: get("audio.language") ?? VOICE_DEFAULTS.language,
  };
}

export function useVoicePrefs(): VoicePrefs {
  const { settings } = useSettings();
  const data = settings.state === "ready" ? settings.data : undefined;
  return useMemo(() => voicePrefsOf(data), [data]);
}

/**
 * Per-device choices, kept in this browser rather than in the daemon: which
 * microphone to use and which built-in voice to speak with are facts about
 * this machine, not about the user's Borg.
 */
export const VOICE_LOCAL_KEYS = {
  microphone: "borg.voice.microphone",
  builtInVoice: "borg.voice.builtInVoice",
} as const;

export function readLocalVoiceChoice(key: keyof typeof VOICE_LOCAL_KEYS): string {
  try {
    return localStorage.getItem(VOICE_LOCAL_KEYS[key]) ?? "";
  } catch {
    return "";
  }
}

export function writeLocalVoiceChoice(key: keyof typeof VOICE_LOCAL_KEYS, value: string): void {
  try {
    if (value === "") localStorage.removeItem(VOICE_LOCAL_KEYS[key]);
    else localStorage.setItem(VOICE_LOCAL_KEYS[key], value);
  } catch {
    // Private mode or a full store: the choice simply does not stick.
  }
}
