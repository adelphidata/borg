import { spawn } from "node:child_process";
import { accessSync, constants, existsSync } from "node:fs";
import net from "node:net";
import path from "node:path";
import { defaultSocketPath, localAddress } from "@borg/client/node";
import type { WireBootStatus } from "./channels";

/**
 * Starting borgd on the user's behalf.
 *
 * The daemon is the app. A window that opens onto "Borg is not running, start
 * it with `borg daemon`" is a terminal instruction wearing a GUI, and the
 * desktop invariant is that the app installs and runs with zero setup. So the
 * shell probes the socket, starts the daemon itself when nothing answers, and
 * reports the wait as a state the renderer can render.
 *
 * The daemon we start is deliberately *not* killed on quit: scheduled tasks,
 * heartbeats, and channel integrations are the product, and closing a window
 * is not a request to stop being an assistant.
 */

/** How long a freshly spawned daemon gets to bind its socket. */
const READY_TIMEOUT_MS = 30_000;
/** Gap between socket probes while waiting. */
const POLL_INTERVAL_MS = 250;
/** How long a single connect attempt may hang before it counts as unreachable. */
const PROBE_TIMEOUT_MS = 1_000;

/**
 * The steps of a boot, as the user sees them.
 *
 * Plain outcomes, no subsystem names: the person waiting does not know what a
 * UDS is and should not have to. Progress is monotonic so the bar only ever
 * moves forward.
 */
export const BOOT_STEPS = {
  probing: { state: "starting", message: "Looking for Borg…", progress: 0.15, canRetry: false },
  starting: { state: "starting", message: "Starting Borg…", progress: 0.45, canRetry: false },
  waiting: { state: "starting", message: "Almost ready…", progress: 0.75, canRetry: false },
  ready: { state: "ready", message: "Ready", progress: 1, canRetry: false },
} as const satisfies Record<string, WireBootStatus>;

/** A boot that ended badly, phrased as what the user can do about it. */
export function bootFailure(message: string): WireBootStatus {
  return { state: "failed", message, progress: 1, canRetry: true };
}

export const NOT_INSTALLED =
  "Borg’s engine is missing from this computer. Reinstall Borg, then try again.";
export const DID_NOT_START = "Borg did not finish starting. Try again, or restart your computer.";

/** Whether a path names a file this process may execute. */
export function isExecutable(file: string): boolean {
  try {
    accessSync(file, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * Where the `borgd` binary lives, in the order worth trying.
 *
 * Mirrors the CLI's own ladder (`crates/interface/src/borgd_launcher.rs`) so a
 * developer's checkout, a packaged install, and a Homebrew install all resolve
 * the same daemon the terminal would have started.
 */
export function borgdCandidates(opts: {
  env: Record<string, string | undefined>;
  /** `app.getAppPath()` — the directory holding the app's package.json. */
  appPath: string;
  /** `process.resourcesPath` in a packaged app. */
  resourcesPath?: string;
  platform: NodeJS.Platform;
  /** Directories from `PATH`, already split. */
  pathDirs: string[];
}): string[] {
  const exe = opts.platform === "win32" ? "borgd.exe" : "borgd";
  const out: string[] = [];
  const override = opts.env.BORG_DAEMON_BIN?.trim();
  if (override) out.push(override);
  if (opts.resourcesPath) out.push(path.join(opts.resourcesPath, "bin", exe));
  // A dev checkout: apps/desktop → repo root → cargo's output directories.
  const repoRoot = path.resolve(opts.appPath, "..", "..");
  out.push(path.join(repoRoot, "target", "debug", exe));
  out.push(path.join(repoRoot, "target", "release", exe));
  for (const dir of opts.pathDirs) {
    if (dir.trim() !== "") out.push(path.join(dir, exe));
  }
  return out;
}

/** The first candidate that exists and can be executed, or null. */
export function resolveBorgd(
  candidates: string[],
  executable: (file: string) => boolean = isExecutable,
): string | null {
  return candidates.find(executable) ?? null;
}

/** Splits a `PATH` value into directories, tolerating an unset variable. */
export function pathDirs(env: Record<string, string | undefined>, platform: NodeJS.Platform) {
  return (env.PATH ?? env.Path ?? "").split(platform === "win32" ? ";" : ":").filter(Boolean);
}

/**
 * True when something is listening on the daemon socket.
 *
 * A connect, not a stat: a socket file left behind by a killed daemon still
 * exists, and starting a second daemon because of a stale inode is exactly the
 * failure this probe is here to prevent.
 */
export function socketReachable(
  socketPath: string,
  timeoutMs = PROBE_TIMEOUT_MS,
): Promise<boolean> {
  if (!existsSync(socketPath)) return Promise.resolve(false);
  // On Windows the file names the daemon's pipe rather than being the socket.
  const address = localAddress(socketPath);
  if (address === null) return Promise.resolve(false);
  return new Promise((resolve) => {
    const socket = net.connect(address);
    const finish = (reachable: boolean) => {
      socket.removeAllListeners();
      socket.destroy();
      resolve(reachable);
    };
    socket.setTimeout(timeoutMs);
    socket.once("connect", () => finish(true));
    socket.once("timeout", () => finish(false));
    socket.once("error", () => finish(false));
  });
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Polls the socket until it answers or the deadline passes. */
export async function waitForSocket(
  socketPath: string,
  timeoutMs: number,
  deps: {
    reachable?: typeof socketReachable;
    now?: () => number;
    wait?: (ms: number) => Promise<unknown>;
  } = {},
): Promise<boolean> {
  const reachable = deps.reachable ?? socketReachable;
  const now = deps.now ?? Date.now;
  const wait = deps.wait ?? sleep;
  const deadline = now() + timeoutMs;
  for (;;) {
    if (await reachable(socketPath)) return true;
    if (now() >= deadline) return false;
    await wait(POLL_INTERVAL_MS);
  }
}

export interface BootDeps {
  env?: Record<string, string | undefined>;
  appPath: string;
  resourcesPath?: string;
  platform?: NodeJS.Platform;
  socketPath?: string;
  /** Injected in tests; production spawns a detached child. */
  launch?: (binary: string, cwd: string | undefined) => Error | null;
  reachable?: typeof socketReachable;
  waitFor?: typeof waitForSocket;
  resolve?: (candidates: string[]) => string | null;
  readyTimeoutMs?: number;
}

/**
 * Spawns `borgd` detached, so it outlives the window that needed it.
 *
 * Output goes nowhere: the daemon writes its own logs under `~/.borg/logs`,
 * and an inherited pipe nobody drains eventually blocks the child.
 */
function launchDetached(binary: string, cwd: string | undefined): Error | null {
  try {
    const child = spawn(binary, [], {
      cwd,
      detached: true,
      stdio: "ignore",
      windowsHide: true,
    });
    child.unref();
    return null;
  } catch (error) {
    return error instanceof Error ? error : new Error(String(error));
  }
}

/** The daemon's own directory, which is where a daemon it spawns should start. */
function borgHome(env: Record<string, string | undefined>): string | undefined {
  const userHome = env.HOME ?? env.USERPROFILE;
  const home = env.BORG_HOME ?? (userHome ? path.join(userHome, ".borg") : undefined);
  return home && existsSync(home) ? home : undefined;
}

/**
 * Brings the daemon up, reporting each step.
 *
 * Resolves once the socket answers or the attempt has failed; either way the
 * final status has already been reported, so callers only have to render it.
 */
export async function ensureDaemonRunning(
  deps: BootDeps,
  report: (status: WireBootStatus) => void,
): Promise<boolean> {
  const env = deps.env ?? process.env;
  const platform = deps.platform ?? process.platform;
  const socketPath = deps.socketPath ?? defaultSocketPath();
  const reachable = deps.reachable ?? socketReachable;
  const waitFor = deps.waitFor ?? waitForSocket;
  const launch = deps.launch ?? launchDetached;

  report(BOOT_STEPS.probing);
  if (await reachable(socketPath)) {
    report(BOOT_STEPS.ready);
    return true;
  }

  const candidates = borgdCandidates({
    env,
    appPath: deps.appPath,
    resourcesPath: deps.resourcesPath,
    platform,
    pathDirs: pathDirs(env, platform),
  });
  const binary = (deps.resolve ?? ((c: string[]) => resolveBorgd(c)))(candidates);
  if (!binary) {
    console.error("[borg] no borgd binary found in:", candidates.join(", "));
    report(bootFailure(NOT_INSTALLED));
    return false;
  }

  report(BOOT_STEPS.starting);
  const failure = launch(binary, borgHome(env));
  if (failure) {
    console.error(`[borg] could not start ${binary}: ${failure.message}`);
    report(bootFailure(DID_NOT_START));
    return false;
  }

  report(BOOT_STEPS.waiting);
  const up = await waitFor(socketPath, deps.readyTimeoutMs ?? READY_TIMEOUT_MS, { reachable });
  if (!up) {
    console.error(`[borg] started ${binary} but its socket never answered`);
    report(bootFailure(DID_NOT_START));
    return false;
  }

  report(BOOT_STEPS.ready);
  return true;
}
