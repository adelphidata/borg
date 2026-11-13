import { useNavigate } from "react-router-dom";
import { useIsMobile } from "@borg/ui";
import { PageGroup } from "../../components/PageGroup";
import { useMeetingsStatus, type MeetingsStatus } from "../meetings/useMeetings";
import { ConnCards, ConnList, type Card } from "./connectionCards";

/** The calendar as a Connections card. */
export function calendarCard(calendar: MeetingsStatus["calendar"]): Card {
  const kind = calendar?.connected ? calendar.kind : "";
  return {
    id: "calendar",
    badge: kind === "google" ? "GC" : kind === "ics" ? "ICS" : "CAL",
    name: kind === "google" ? "Google Calendar" : kind === "ics" ? "Calendar feed" : "Calendar",
    sub: calendar?.connected ? calendar.accountLabel || "Connected" : "Not connected",
    tone: calendar?.connected ? "ok" : "idle",
  };
}

/**
 * The calendar Meetings reads, at the top of Connections — once Meetings is
 * on, or whenever a calendar is connected. A daemon without Meetings shows
 * nothing here rather than an error.
 */
export function CalendarSection() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const status = useMeetingsStatus();
  if (status.state !== "ready") return null;
  const { enabled, calendar } = status.data;
  if (!enabled && !calendar?.connected) return null;
  const cards = [calendarCard(calendar)];
  const open = () => navigate("/connections/calendar");
  return (
    <PageGroup title="Calendar">
      {isMobile ? <ConnList cards={cards} open={open} /> : <ConnCards cards={cards} open={open} />}
    </PageGroup>
  );
}
