import { useEffect, useRef, useState } from "react";
import { MicIcon } from "@borg/ui";
import { asBorgError, useBorg } from "../../lib/useDaemon";
import { useVoiceCatalog } from "../../lib/voice";

type Phase = "idle" | "recording" | "transcribing";

/** Whether this browser can record at all. */
function canRecord(): boolean {
  return (
    typeof MediaRecorder !== "undefined" &&
    typeof navigator !== "undefined" &&
    typeof navigator.mediaDevices?.getUserMedia === "function"
  );
}

/**
 * The composer's microphone: click to record, click again to turn the
 * recording into text through the daemon's transcription chain.
 *
 * It renders nothing unless transcription is set up and working — a
 * microphone that can only answer "voice input is off" is a control that
 * should not be at the door. Audio goes to the daemon, never straight to a
 * provider, so keys stay where they are kept.
 */
export function VoiceInputButton({
  onText,
  onError,
}: {
  /** The recognized words, trimmed and non-empty. */
  onText: (text: string) => void;
  /** A plain-words failure; `undefined` clears an earlier one. */
  onError: (message: string | undefined) => void;
}) {
  const borg = useBorg();
  const catalog = useVoiceCatalog();
  const [phase, setPhase] = useState<Phase>("idle");
  const recorder = useRef<MediaRecorder | null>(null);
  // Set from the click until the recorder runs, so a second click during the
  // permission prompt cannot open a second stream.
  const starting = useRef(false);
  // Read by async work that outlives the render it was made in.
  const live = useRef(true);
  const handlers = useRef({ onText, onError });
  handlers.current = { onText, onError };

  // A failed refresh keeps the last answer: a network blip must not cut off a recording.
  const lastReady = useRef(false);
  if (catalog.state === "ready") lastReady.current = catalog.data.transcriptionReady;
  const visible = canRecord() && lastReady.current;
  // Read after the permission prompt, which can outlast the button.
  const visibleRef = useRef(visible);
  visibleRef.current = visible;

  /**
   * End a recording without sending it: the microphone goes off, the audio is
   * dropped. True when there was a recording to drop.
   */
  const discard = () => {
    const r = recorder.current;
    recorder.current = null;
    if (!r) return false;
    r.onstop = null;
    if (r.state !== "inactive") r.stop();
    r.stream.getTracks().forEach((t) => t.stop());
    return true;
  };

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
      // Leaving the page must not leave the microphone on, nor send what it heard.
      discard();
    };
  }, []);

  // Voice input turned off mid-recording: no button is left to stop it, so stop it here.
  useEffect(() => {
    if (!visible) {
      if (discard())
        handlers.current.onError("Voice input was turned off, so the recording was dropped.");
      setPhase("idle");
    }
  }, [visible]);

  if (!visible) return null;

  const transcribe = async (blob: Blob) => {
    setPhase("transcribing");
    try {
      const audio = new Uint8Array(await blob.arrayBuffer());
      const { text } = await borg.voice.transcribe({
        audio,
        mimeType: blob.type,
        language: "",
      });
      if (!live.current) return;
      if (text.trim() === "")
        handlers.current.onError("Didn't catch that. Try again a bit closer.");
      else handlers.current.onText(text.trim());
    } catch (e) {
      if (live.current) handlers.current.onError(asBorgError(e).message);
    } finally {
      if (live.current) setPhase("idle");
    }
  };

  const start = async () => {
    if (starting.current || recorder.current) return;
    starting.current = true;
    handlers.current.onError(undefined);
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch {
      starting.current = false;
      if (live.current)
        handlers.current.onError(
          "Borg can't use the microphone. Allow it for this app and try again.",
        );
      return;
    }
    starting.current = false;
    // The composer went away, or voice input was turned off, while the
    // permission prompt was open.
    if (!live.current || !visibleRef.current) {
      stream.getTracks().forEach((t) => t.stop());
      return;
    }
    const r = new MediaRecorder(stream);
    const chunks: Blob[] = [];
    r.ondataavailable = (e) => {
      if (e.data.size > 0) chunks.push(e.data);
    };
    r.onstop = () => {
      stream.getTracks().forEach((t) => t.stop());
      recorder.current = null;
      void transcribe(new Blob(chunks, { type: r.mimeType || "audio/webm" }));
    };
    recorder.current = r;
    r.start();
    setPhase("recording");
  };

  const hint =
    phase === "recording"
      ? "Stop recording"
      : phase === "transcribing"
        ? "Transcribing…"
        : "Speak instead of typing";

  return (
    <button
      type="button"
      // One stable name; the state rides on pressed/busy, as screen readers expect.
      aria-label="Voice input"
      title={hint}
      aria-pressed={phase === "recording"}
      aria-busy={phase === "transcribing"}
      disabled={phase === "transcribing"}
      onClick={() => (phase === "recording" ? recorder.current?.stop() : void start())}
      className={`flex h-[29px] w-[29px] flex-none cursor-pointer items-center justify-center rounded-[9px] border-0 disabled:cursor-wait ${
        phase === "recording"
          ? "animate-pulse bg-alert text-white"
          : "bg-transparent text-ink3 hover:text-ink2"
      } ${phase === "transcribing" ? "opacity-50" : ""}`}
    >
      <MicIcon size={15} />
    </button>
  );
}
