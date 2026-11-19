import type { meetings } from "@borg/client";
import { segmentClock } from "./meetingsFormat";

/** Who a transcript line came from: the microphone is the user. */
export function speakerOf(track: string): string {
  return track === "mic" ? "Me" : "Them";
}

/** The raw transcript, line by line: when, who, what was said. */
export function TranscriptList({ segments }: { segments: meetings.Segment[] }) {
  return (
    <ol aria-label="Transcript" className="m-0 flex list-none flex-col gap-2.5 p-0">
      {segments.map((s, i) => (
        <li
          key={`${s.track}:${s.startMs}:${i}`}
          className="flex gap-3 text-[13.5px] leading-[1.55]"
        >
          <span className="w-[42px] flex-none text-ink3 tabular-nums">
            {segmentClock(s.startMs)}
          </span>
          <span
            className={`w-[40px] flex-none font-medium ${s.track === "mic" ? "text-ink" : "text-ink2"}`}
          >
            {speakerOf(s.track)}
          </span>
          <span className="min-w-0 flex-1 text-ink select-text">{s.text}</span>
        </li>
      ))}
    </ol>
  );
}
