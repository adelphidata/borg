import { useState } from "react";
import type { meetings } from "@borg/client";
import { plural } from "../../lib/format";
import { segmentClock } from "./meetingsFormat";

/** Who a transcript line came from: the microphone is the user. */
export function speakerOf(track: string): string {
  return track === "mic" ? "Me" : "Them";
}

/**
 * The raw transcript, folded away. The summary is what people read; the
 * transcript is there to check a quote, so it costs one click.
 */
export function TranscriptFold({ segments }: { segments: meetings.Segment[] }) {
  const [open, setOpen] = useState(false);
  if (segments.length === 0) return null;
  return (
    <section className="flex flex-col gap-3">
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="flex w-full cursor-pointer items-center gap-2 border-0 bg-transparent p-0 text-left text-[12.5px] leading-none font-medium text-ink2"
      >
        <span
          aria-hidden="true"
          className={`inline-block transition-transform ${open ? "rotate-90" : ""}`}
        >
          ›
        </span>
        Transcript · {plural(segments.length, "line")}
      </button>
      {open && (
        <ol className="m-0 flex list-none flex-col gap-2 p-0">
          {segments.map((s, i) => (
            <li
              key={`${s.track}:${s.startMs}:${i}`}
              className="flex gap-3 text-[13px] leading-[1.5]"
            >
              <span className="w-[42px] flex-none text-ink3 tabular-nums">
                {segmentClock(s.startMs)}
              </span>
              <span className="w-[38px] flex-none font-medium text-ink2">{speakerOf(s.track)}</span>
              <span className="min-w-0 flex-1 text-ink select-text">{s.text}</span>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
