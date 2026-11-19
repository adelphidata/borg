import type { meetings } from "@borg/client";

/**
 * How the Meetings page words and groups things. Pure: every function takes
 * `now` and `locale` where the answer depends on them, so a test pins both.
 */

type Attendee = meetings.Attendee;
type CalendarEvent = meetings.CalendarEvent;
type MeetingSummary = meetings.MeetingSummary;

/** A person's first name, or the part of their address before the @. */
function firstName(a: Attendee): string {
  const name = a.name.trim();
  if (name !== "") return name.split(/\s+/)[0]!;
  return a.email.split("@")[0] ?? "";
}

/**
 * Who was there, the way Granola says it: "Juan", "Juan & Jae",
 * "Juan, Jae, & Ali", and past three "Saad, Ali, & 10 others".
 */
export function attendeeLine(attendees: Attendee[]): string {
  const names = attendees.map(firstName).filter((n) => n !== "");
  if (names.length <= 1) return names[0] ?? "";
  if (names.length === 2) return `${names[0]} & ${names[1]}`;
  if (names.length === 3) return `${names[0]}, ${names[1]}, & ${names[2]}`;
  return `${names[0]}, ${names[1]}, & ${names.length - 2} others`;
}

/** A clock time split into its digits and its AM/PM, when the locale has one. */
function clockParts(at: Date, locale?: string): { time: string; period: string } {
  const parts = new Intl.DateTimeFormat(locale, {
    hour: "numeric",
    minute: "2-digit",
  }).formatToParts(at);
  const period = parts.find((p) => p.type === "dayPeriod")?.value ?? "";
  const time = parts
    .filter((p) => p.type !== "dayPeriod")
    .map((p) => p.value)
    .join("")
    .trim();
  return { time, period };
}

/**
 * "9:45 – 10:15" with "AM" as a separate suffix the page sets small, when
 * both ends share a period; "9:45 AM – 1:15 PM" with no suffix when they do
 * not. A 24-hour locale has no period, so no suffix.
 */
export function timeRange(
  startUnix: number,
  endUnix: number,
  locale?: string,
): { text: string; suffix: string } {
  const a = clockParts(new Date(startUnix * 1000), locale);
  const b = clockParts(new Date(endUnix * 1000), locale);
  if (a.period === b.period) return { text: `${a.time} – ${b.time}`, suffix: a.period };
  return { text: `${a.time} ${a.period} – ${b.time} ${b.period}`, suffix: "" };
}

/** "4:00 PM": when a past meeting started, inside a day the list already names. */
export function timeOfDay(unix: number, locale?: string): string {
  return new Date(unix * 1000).toLocaleTimeString(locale, { hour: "numeric", minute: "2-digit" });
}

/** Local midnight of the day `at` falls on. */
export function startOfDay(at: Date): Date {
  return new Date(at.getFullYear(), at.getMonth(), at.getDate());
}

/** `days` whole days after `at`, by the calendar rather than by 24-hour steps. */
export function addDays(at: Date, days: number): Date {
  return new Date(at.getFullYear(), at.getMonth(), at.getDate() + days);
}

const sameDay = (a: Date, b: Date) => a.toDateString() === b.toDateString();

/** One row of the "Coming up" card. */
export interface DayRow {
  /** Local midnight of the day. */
  day: Date;
  isToday: boolean;
  /** What is still ahead on this day, in start order. */
  events: CalendarEvent[];
  /** Today, with every event already over: the row says so instead. */
  noMoreToday: boolean;
}

/**
 * The days of the `days`-long window from `windowStart` worth a row: today
 * whenever it is in the window, and every other day that has an event. An
 * event that began before the window and is still running sits on its first
 * day. Today's finished events drop off.
 */
export function dayRows(events: CalendarEvent[], windowStart: Date, now: Date, days = 7): DayRow[] {
  const first = startOfDay(windowStart);
  const end = addDays(first, days);
  const today = startOfDay(now);
  const rows = new Map<number, DayRow>();
  const rowFor = (day: Date) => {
    let row = rows.get(day.getTime());
    if (!row) {
      const isToday = sameDay(day, today);
      row = { day, isToday, events: [], noMoreToday: false };
      rows.set(day.getTime(), row);
    }
    return row;
  };
  if (today >= first && today < end) rowFor(today);
  const sorted = [...events].sort((a, b) => a.startUnix - b.startUnix);
  for (const event of sorted) {
    const starts = startOfDay(new Date(event.startUnix * 1000));
    const day = starts < first ? first : starts;
    if (day >= end) continue;
    if (event.endUnix * 1000 <= first.getTime()) continue;
    const row = rowFor(day);
    if (row.isToday && event.endUnix * 1000 <= now.getTime()) continue;
    row.events.push(event);
  }
  for (const row of rows.values()) row.noMoreToday = row.isToday && row.events.length === 0;
  return [...rows.values()]
    .filter((r) => r.isToday || r.events.length > 0)
    .sort((a, b) => a.day.getTime() - b.day.getTime());
}

/** One day's worth of past meetings, newest first. */
export interface PastGroup {
  /** "Today", "Yesterday", or "Monday, Nov 10". */
  label: string;
  meetings: MeetingSummary[];
}

/** The label a past day goes under. Boundaries are local midnight. */
export function dayLabel(at: Date, now: Date, locale?: string): string {
  const day = startOfDay(at);
  const today = startOfDay(now);
  if (day.getTime() === today.getTime()) return "Today";
  if (day.getTime() === addDays(today, -1).getTime()) return "Yesterday";
  return at.toLocaleDateString(locale, { weekday: "long", month: "short", day: "numeric" });
}

/** Past meetings under their day labels, keeping the order they came in. */
export function groupPast(list: MeetingSummary[], now: Date, locale?: string): PastGroup[] {
  const groups: PastGroup[] = [];
  for (const m of list) {
    const label = dayLabel(new Date(m.startedAt * 1000), now, locale);
    const last = groups[groups.length - 1];
    if (last?.label === label) last.meetings.push(m);
    else groups.push({ label, meetings: [m] });
  }
  return groups;
}

/** The letter on a row's tile: the first attendee's, else the title's. */
export function initialOf(m: Pick<MeetingSummary, "attendees" | "title">): string {
  const who = m.attendees.map(firstName).find((n) => n !== "");
  const source = who ?? m.title.trim();
  return (source.charAt(0) || "?").toUpperCase();
}

/** A transcript line's offset into the meeting: "4:07", "12:04", "1:02:03". */
export function segmentClock(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

/** "32 min", "1 h", "1 h 5 min"; empty while the meeting has no end yet. */
export function duration(startUnix: number, endUnix: number): string {
  if (endUnix <= startUnix) return "";
  const mins = Math.max(1, Math.round((endUnix - startUnix) / 60));
  const hours = Math.floor(mins / 60);
  const rest = mins % 60;
  return hours === 0 ? `${mins} min` : rest === 0 ? `${hours} h` : `${hours} h ${rest} min`;
}

/** "Today", "Yesterday", "Tomorrow", else "Thu, Nov 12": a chip's day. */
export function shortDay(at: Date, now: Date, locale?: string): string {
  const days = Math.round((startOfDay(at).getTime() - startOfDay(now).getTime()) / 86_400_000);
  if (days === 0) return "Today";
  if (days === -1) return "Yesterday";
  if (days === 1) return "Tomorrow";
  return at.toLocaleDateString(locale, { weekday: "short", month: "short", day: "numeric" });
}

/** "Today · 9:45 – 10:15 AM", "Fri, Nov 14 · All day": when an event is. */
export function eventWhen(
  e: Pick<CalendarEvent, "startUnix" | "endUnix" | "allDay">,
  now: Date,
  locale?: string,
): string {
  const day = shortDay(new Date(e.startUnix * 1000), now, locale);
  if (e.allDay) return `${day} · All day`;
  const range = timeRange(e.startUnix, e.endUnix, locale);
  return `${day} · ${range.text}${range.suffix ? ` ${range.suffix}` : ""}`;
}

/** Everyone by full name, or address when there is no name. */
export function fullNames(attendees: Attendee[]): string[] {
  return attendees.map((a) => a.name.trim() || a.email.trim()).filter((n) => n !== "");
}
