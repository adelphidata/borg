import { useCallback, useEffect, useRef, useSyncExternalStore } from "react";
import { useBorg } from "./useDaemon";
import type { BorgClient } from "@borg/client";
import { readLocalVoiceChoice, useVoiceCatalog } from "./voice";

/**
 * Whether the daemon's speech chain is ready, as the last catalog read had
 * it. Held here rather than asked per button: a transcript has one speaker
 * button per reply, and each asking the daemon would be a request storm.
 */
let daemonReady = false;
/** The client the page-level hook lent the speaker, for daemon synthesis. */
let client: BorgClient | undefined;

/**
 * Spoken replies.
 *
 * One speaker for the whole app, because two replies talking over each other
 * is noise: the store below holds what is being said, every read-aloud button
 * subscribes to it, and starting one stops the other.
 *
 * Two ways to make sound. A speech provider picked in Settings synthesizes
 * through the daemon (`Voice.Synthesize`), so keys stay there. With none
 * picked, the device's own voice (`speechSynthesis`) reads the text — free,
 * offline, and already on every phone and laptop, which is why it is the
 * default and why a reply can be heard before anything is set up.
 */

export type SpeechState = { id: string; phase: "loading" | "speaking" } | undefined;

let current: SpeechState;
/** What is playing now, so `stop` can end it. */
let playing: { kind: "audio"; el: HTMLAudioElement; url: string } | { kind: "device" } | undefined;
/** Counts speak calls; a reply that arrives for a superseded call is dropped. */
let generation = 0;
const listeners = new Set<() => void>();

function set(next: SpeechState) {
  current = next;
  for (const l of listeners) l();
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Stops whatever is speaking. Safe to call when nothing is. */
export function stopSpeaking(): void {
  generation += 1;
  const p = playing;
  playing = undefined;
  if (p?.kind === "audio") {
    // Emptying `src` makes the element fire `error`; that is us stopping it,
    // not a failure to report against the reply.
    p.el.onended = null;
    p.el.onerror = null;
    p.el.pause();
    p.el.src = "";
    URL.revokeObjectURL(p.url);
  } else if (p?.kind === "device") {
    globalThis.speechSynthesis?.cancel();
  }
  if (current !== undefined) set(undefined);
}

/** True where the device can speak on its own. */
export function hasDeviceVoice(): boolean {
  return (
    typeof globalThis.speechSynthesis !== "undefined" &&
    typeof globalThis.SpeechSynthesisUtterance !== "undefined"
  );
}

/** Enough markdown removed for a voice to read it as prose. */
export function speakableText(markdown: string): string {
  return markdown
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/`([^`]*)`/g, "$1")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, " ")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/https?:\/\/\S+/g, "")
    .replace(/^[ \t]{0,3}#{1,6}[ \t]+/gm, "")
    .replace(/^[ \t]*[-*+][ \t]+/gm, "")
    .replace(/^[ \t]*>[ \t]?/gm, "")
    .replace(/(\*\*|__)(.*?)\1/g, "$2")
    .replace(/(\*|_)(.*?)\1/g, "$2")
    .replace(/~~(.*?)~~/g, "$1")
    .replace(/[ \t]+/g, " ")
    .replace(/ \n/g, "\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

/** The device voice chosen in Settings, if it is still installed. */
function deviceVoice(): SpeechSynthesisVoice | undefined {
  const name = readLocalVoiceChoice("builtInVoice");
  if (!name) return undefined;
  return globalThis.speechSynthesis.getVoices().find((v) => v.name === name);
}

function speakOnDevice(id: string, text: string, onError: (message: string) => void) {
  const gen = generation;
  const utterance = new SpeechSynthesisUtterance(text);
  const voice = deviceVoice();
  if (voice) utterance.voice = voice;
  utterance.onend = () => {
    if (gen === generation) {
      playing = undefined;
      set(undefined);
    }
  };
  utterance.onerror = (e) => {
    if (gen !== generation) return;
    playing = undefined;
    set(undefined);
    // Cancelling our own utterance reports as an error; that is not news.
    if (e.error !== "interrupted" && e.error !== "canceled") {
      onError("This device could not read that aloud.");
    }
  };
  playing = { kind: "device" };
  set({ id, phase: "speaking" });
  globalThis.speechSynthesis.speak(utterance);
}

async function speakThroughDaemon(
  borg: BorgClient,
  id: string,
  text: string,
  onError: (message: string) => void,
) {
  const gen = generation;
  set({ id, phase: "loading" });
  let reply: { audio: Uint8Array; mimeType: string };
  try {
    reply = await borg.voice.synthesize({ text, voice: "" });
  } catch (e) {
    if (gen !== generation) return;
    set(undefined);
    onError(e instanceof Error ? e.message : "Could not read that aloud.");
    return;
  }
  if (gen !== generation) return;
  const url = URL.createObjectURL(new Blob([reply.audio as BlobPart], { type: reply.mimeType }));
  const el = new Audio(url);
  const done = () => {
    if (gen !== generation) return;
    playing = undefined;
    URL.revokeObjectURL(url);
    set(undefined);
  };
  el.onended = done;
  el.onerror = () => {
    // A stop or a newer reply already took over; nothing to report.
    if (gen !== generation) return;
    done();
    onError("Could not play the reply.");
  };
  playing = { kind: "audio", el, url };
  set({ id, phase: "speaking" });
  try {
    await el.play();
  } catch {
    done();
    onError("The browser blocked playback. Click the speaker to hear it.");
  }
}

export interface Speaker {
  /** The reply being spoken, or nothing. */
  state: SpeechState;
  /** True when something here can make sound at all. */
  canSpeak: boolean;
  /** Reads `text` aloud as `id`; a second call with the same id stops it. */
  speak: (id: string, text: string) => void;
  stop: () => void;
}

/**
 * The speaker, for any component with a reply to read. `onError` gets a
 * plain-words failure and is optional: auto-speak has nowhere to show one.
 */
export function useSpeaker(onError?: (message: string) => void): Speaker {
  const state = useSyncExternalStore(
    subscribe,
    () => current,
    () => current,
  );
  const viaDaemon = useSyncExternalStore(
    subscribe,
    () => daemonReady,
    () => daemonReady,
  );
  const canSpeak = viaDaemon || hasDeviceVoice();
  // `onError` is a fresh arrow per render; the ref keeps `speak` stable
  // while a failure still reaches the latest one.
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;

  const speak = useCallback(
    (id: string, markdown: string) => {
      if (current?.id === id) {
        stopSpeaking();
        return;
      }
      stopSpeaking();
      const text = speakableText(markdown);
      if (text === "") return;
      const report = (message: string) => onErrorRef.current?.(message);
      // The daemon strips markdown itself, and keeps the structure it needs
      // to split sentences; the device voice gets the plain prose.
      if (viaDaemon && client) void speakThroughDaemon(client, id, markdown, report);
      else if (hasDeviceVoice()) speakOnDevice(id, text, report);
    },
    [viaDaemon],
  );

  return { state, canSpeak, speak, stop: stopSpeaking };
}

/**
 * Keeps the speaker told whether the daemon can synthesize, and lends it the
 * client to do so. One page-level call; every `useSpeaker` below it follows,
 * which is what lets a reply card speak without a daemon context of its own.
 */
export function useSpeechSource(): void {
  const borg = useBorg();
  const catalog = useVoiceCatalog();
  const ready = catalog.state === "ready" ? catalog.data.speechReady : undefined;
  useEffect(() => {
    client = borg;
    if (ready === undefined || ready === daemonReady) return;
    daemonReady = ready;
    for (const l of listeners) l();
  }, [borg, ready]);
}

/**
 * Reads each reply aloud as its turn ends, when Settings asks for it. Only
 * the reply that just finished: an older one is not news, and a reply that
 * ended in an error has a card to say so.
 */
export function useAutoSpeak(
  enabled: boolean,
  busy: boolean,
  lastReply: { id: string; text: string } | undefined,
) {
  const speaker = useSpeaker();
  const wasBusy = useWasBusy(busy);
  useEffect(() => {
    if (!enabled || !wasBusy || busy || !lastReply) return;
    speaker.speak(lastReply.id, lastReply.text);
    // Fires on the busy→idle edge only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [busy]);
}

/** What `busy` was on the previous render. */
function useWasBusy(busy: boolean): boolean {
  const prev = useRef(busy);
  const was = prev.current;
  useEffect(() => {
    prev.current = busy;
  }, [busy]);
  return was;
}

/** Test seam: forgets what is speaking and where sound comes from. */
export function resetSpeaker(): void {
  stopSpeaking();
  daemonReady = false;
  client = undefined;
}
