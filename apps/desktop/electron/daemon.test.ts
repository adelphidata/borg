import { describe, expect, it, vi } from "vitest";
import path from "node:path";
import {
  BOOT_STEPS,
  DID_NOT_START,
  NOT_INSTALLED,
  borgdCandidates,
  ensureDaemonRunning,
  pathDirs,
  resolveBorgd,
  waitForSocket,
  type BootDeps,
} from "./daemon";
import type { WireBootStatus } from "./channels";

const APP_PATH = "/Apps/Borg.app/Contents/Resources/app/apps/desktop";

describe("borgdCandidates", () => {
  it("prefers an explicit override over everything the ladder would find", () => {
    const candidates = borgdCandidates({
      env: { BORG_DAEMON_BIN: "/custom/borgd", PATH: "/usr/bin" },
      appPath: APP_PATH,
      resourcesPath: "/Apps/Borg.app/Contents/Resources",
      platform: "darwin",
      pathDirs: ["/usr/bin"],
    });
    expect(candidates[0]).toBe("/custom/borgd");
  });

  it("looks inside the packaged app before anything on the machine", () => {
    // A shipped daemon must win: an old `borgd` on PATH would otherwise pair a
    // new app with a daemon that predates its protos.
    const candidates = borgdCandidates({
      env: { PATH: "/usr/local/bin" },
      appPath: APP_PATH,
      resourcesPath: "/Apps/Borg.app/Contents/Resources",
      platform: "darwin",
      pathDirs: ["/usr/local/bin"],
    });
    expect(candidates[0]).toBe("/Apps/Borg.app/Contents/Resources/bin/borgd");
    expect(candidates.indexOf("/usr/local/bin/borgd")).toBeGreaterThan(0);
  });

  it("finds a developer's cargo build from the app directory", () => {
    const candidates = borgdCandidates({
      env: {},
      appPath: "/repo/apps/desktop",
      platform: "linux",
      pathDirs: [],
    });
    expect(candidates).toContain(path.join("/repo", "target", "debug", "borgd"));
    expect(candidates).toContain(path.join("/repo", "target", "release", "borgd"));
  });

  it("asks for the right file name on Windows", () => {
    const candidates = borgdCandidates({
      env: {},
      appPath: "C:\\repo\\apps\\desktop",
      platform: "win32",
      pathDirs: ["C:\\bin"],
    });
    expect(candidates.every((c) => c.endsWith("borgd.exe"))).toBe(true);
  });
});

describe("pathDirs", () => {
  it("splits on the platform's separator", () => {
    expect(pathDirs({ PATH: "/a:/b" }, "darwin")).toEqual(["/a", "/b"]);
    expect(pathDirs({ Path: "C:\\a;C:\\b" }, "win32")).toEqual(["C:\\a", "C:\\b"]);
  });

  it("yields nothing rather than an empty directory when PATH is unset", () => {
    // An empty entry would resolve to a bare "borgd" relative path.
    expect(pathDirs({}, "darwin")).toEqual([]);
  });
});

describe("resolveBorgd", () => {
  it("skips a path that exists but cannot be executed", () => {
    const executable = (file: string) => file === "/usr/bin/borgd";
    expect(resolveBorgd(["/broken/borgd", "/usr/bin/borgd"], executable)).toBe("/usr/bin/borgd");
  });

  it("reports nothing found rather than guessing", () => {
    expect(resolveBorgd(["/a", "/b"], () => false)).toBeNull();
  });
});

describe("waitForSocket", () => {
  it("returns as soon as the socket answers", async () => {
    const reachable = vi.fn().mockResolvedValueOnce(false).mockResolvedValueOnce(true);
    const up = await waitForSocket("/s.sock", 5_000, {
      reachable,
      now: () => 0,
      wait: async () => {},
    });
    expect(up).toBe(true);
    expect(reachable).toHaveBeenCalledTimes(2);
  });

  it("gives up at the deadline instead of polling forever", async () => {
    let clock = 0;
    const up = await waitForSocket("/s.sock", 1_000, {
      reachable: async () => false,
      now: () => (clock += 600),
      wait: async () => {},
    });
    expect(up).toBe(false);
  });
});

/** Collects the statuses a boot reports, in order. */
function recorder() {
  const seen: WireBootStatus[] = [];
  return { seen, report: (s: WireBootStatus) => void seen.push(s) };
}

const base = (over: Partial<BootDeps> = {}): BootDeps => ({
  appPath: "/repo/apps/desktop",
  env: { PATH: "/usr/bin", HOME: "/home/me" },
  platform: "linux",
  socketPath: "/home/me/.borg/borgd.sock",
  resolve: () => "/usr/bin/borgd",
  launch: () => null,
  reachable: async () => false,
  waitFor: async () => true,
  ...over,
});

describe("ensureDaemonRunning", () => {
  it("does not start a second daemon when one already answers", async () => {
    const launch = vi.fn(() => null);
    const { seen, report } = recorder();
    const ok = await ensureDaemonRunning(base({ reachable: async () => true, launch }), report);
    expect(ok).toBe(true);
    expect(launch).not.toHaveBeenCalled();
    expect(seen.at(-1)).toEqual(BOOT_STEPS.ready);
  });

  it("starts the daemon and reports ready once its socket answers", async () => {
    const launch = vi.fn(() => null);
    const { seen, report } = recorder();
    const ok = await ensureDaemonRunning(base({ launch }), report);
    expect(ok).toBe(true);
    expect(launch).toHaveBeenCalledWith("/usr/bin/borgd", undefined);
    expect(seen.map((s) => s.state)).toEqual(["starting", "starting", "starting", "ready"]);
    // The bar only ever moves forward while the user is watching it.
    const progress = seen.map((s) => s.progress);
    expect([...progress].sort((a, b) => a - b)).toEqual(progress);
  });

  it("says the engine is missing when no binary can be found", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { seen, report } = recorder();
    const ok = await ensureDaemonRunning(base({ resolve: () => null }), report);
    expect(ok).toBe(false);
    expect(seen.at(-1)).toMatchObject({ state: "failed", message: NOT_INSTALLED, canRetry: true });
  });

  it("reports a failed spawn as a retryable failure", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { seen, report } = recorder();
    const ok = await ensureDaemonRunning(base({ launch: () => new Error("EACCES") }), report);
    expect(ok).toBe(false);
    expect(seen.at(-1)).toMatchObject({ state: "failed", message: DID_NOT_START });
  });

  it("does not claim success when the daemon never binds its socket", async () => {
    // The spawn succeeding proves nothing; only the socket does.
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { seen, report } = recorder();
    const ok = await ensureDaemonRunning(base({ waitFor: async () => false }), report);
    expect(ok).toBe(false);
    expect(seen.at(-1)).toMatchObject({ state: "failed", message: DID_NOT_START });
  });

  it("keeps paths and commands out of what the user is shown", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    const { seen, report } = recorder();
    await ensureDaemonRunning(base({ resolve: () => null }), report);
    for (const status of seen) {
      expect(status.message).not.toMatch(/\/|\\|borgd|`/);
    }
  });
});
