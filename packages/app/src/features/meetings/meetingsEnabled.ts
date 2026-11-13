import type { DaemonSetting } from "../../lib/settings";

/** The setting that turns the whole feature on. */
export const MEETINGS_ENABLED_KEY = "meetings.enabled";

/**
 * Whether Meetings is on. Off until the snapshot says otherwise: the nav item
 * and the event feed should not flash in while settings are still loading.
 */
export function meetingsEnabled(settings: Map<string, DaemonSetting> | undefined): boolean {
  return settings?.get(MEETINGS_ENABLED_KEY)?.value === "true";
}
