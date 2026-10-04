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
