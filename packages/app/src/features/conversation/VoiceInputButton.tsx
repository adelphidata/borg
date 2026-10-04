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
  // Read by the recorder's stop handler, which outlives the render it was made in.
  const live = useRef(true);
  const handlers = useRef({ onText, onError });
  handlers.current = { onText, onError };

  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
      // Leaving the page must not leave the microphone on.
      const r = recorder.current;
      if (r && r.state !== "inactive") r.stop();
      r?.stream.getTracks().forEach((t) => t.stop());
    };
  }, []);

  if (!canRecord() || catalog.state !== "ready" || !catalog.data.transcriptionReady) return null;

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
    handlers.current.onError(undefined);
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch {
      handlers.current.onError(
        "Borg can't use the microphone. Allow it for this app and try again.",
      );
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

  const label =
    phase === "recording"
      ? "Stop recording"
      : phase === "transcribing"
        ? "Transcribing…"
        : "Speak instead of typing";

  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      aria-pressed={phase === "recording"}
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
