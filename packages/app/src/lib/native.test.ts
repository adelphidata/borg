import { describe, expect, it, vi } from "vitest";
import { NO_NATIVE, type BootStatus } from "./native";

describe("NO_NATIVE", () => {
  it("reports Borg as already running on a surface that cannot start it", () => {
    // The browser reached the gateway to load the page at all. If its stub
    // stayed silent, the boot screen would hang forever on the web surface.
    const seen: BootStatus[] = [];
    NO_NATIVE.boot.subscribe((status) => seen.push(status));
    expect(seen).toEqual([{ state: "ready", message: "Ready", progress: 1, canRetry: false }]);
  });

  it("hands back an unsubscribe like every other subscription", () => {
    expect(() => NO_NATIVE.boot.subscribe(vi.fn())()).not.toThrow();
  });

  it("ignores a retry rather than throwing at a caller that cannot know", () => {
    expect(() => NO_NATIVE.boot.retry()).not.toThrow();
  });

  it("tells a meeting panel there is no call to show, rather than leaving it waiting", () => {
    // The panel renders nothing until it hears; silence would be a blank box.
    const seen: unknown[] = [];
    NO_NATIVE.meetings.onDetection((d) => seen.push(d));
    expect(seen).toEqual([null]);
  });
});
