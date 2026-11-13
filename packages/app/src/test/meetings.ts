import type { meetings } from "@borg/client";

/**
 * Builders for the Meetings tests.
 */

export function event(over: Partial<meetings.CalendarEvent> = {}): meetings.CalendarEvent {
  return {
    id: "e1",
    title: "Design review",
    startUnix: 0,
    endUnix: 0,
    attendees: [],
    joinUrl: "",
    provider: "",
    allDay: false,
    ...over,
  };
}

export function meeting(over: Partial<meetings.MeetingSummary> = {}): meetings.MeetingSummary {
  return {
    id: "m1",
    title: "Weekly sync",
    startedAt: 0,
    endedAt: 0,
    state: "done",
    sourceApp: "",
    attendees: [],
    unread: false,
    hasSummary: false,
    calendarEventId: "",
    ...over,
  };
}

export function status(over: Partial<meetings.MeetingsStatus> = {}): meetings.MeetingsStatus {
  return {
    enabled: true,
    transcriptionReady: true,
    detectionSupported: true,
    calendar: { connected: false, kind: "", accountLabel: "" },
    recordingsDir: "~/.borg/recordings/meetings",
    unreadCount: 0,
    ...over,
  };
}

export function fullMeeting(
  over: Partial<meetings.MeetingSummary> = {},
  rest: Partial<Omit<meetings.Meeting, "summary">> = {},
): meetings.Meeting {
  return {
    summary: meeting(over),
    segments: [],
    notes: "",
    summaryMarkdown: "",
    error: "",
    dir: "/tmp/m1",
    ...rest,
  };
}

/** One `Admin.GetSettings` entry. */
export function entry(key: string, value: string) {
  return { key, value, category: "db", label: "", kind: "", group: "" };
}

/** A `Voice.GetVoiceCatalog` provider. */
export function voiceProvider(id: string, label: string, extra: Record<string, unknown> = {}) {
  return {
    id,
    label,
    needsKey: true,
    keyUrl: `https://${id}.example/keys`,
    defaultModel: "",
    defaultVoice: "",
    requiresCommand: false,
    hasKey: false,
    keyLabel: `${label} API key`,
    ...extra,
  };
}

/** A voice catalog whose transcribers are `transcription`. */
export function voiceCatalog(transcription: ReturnType<typeof voiceProvider>[], ready = false) {
  return {
    speech: [],
    transcription,
    phone: [],
    speechReady: false,
    transcriptionReady: ready,
    phoneProblem: "",
  };
}
