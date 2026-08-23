import { beforeEach, describe, expect, it, vi } from "vitest";
import { EventEmitter } from "node:events";

/**
 * The parts of `daemon.ts` that touch the machine: the executable probe, the
 * socket connect, the detached spawn, and the working directory the daemon is
 * started in. The rest of the module takes its dependencies as arguments and is
 * covered in `daemon.test.ts`.
 */
const fs = vi.hoisted(() => ({
  accessSync: vi.fn(),
  existsSync: vi.fn(() => true),
  constants: { X_OK: 1 },
}));
vi.mock("node:fs", () => ({ ...fs, default: fs }));

const child = vi.hoisted(() => ({ spawn: vi.fn() }));
vi.mock("node:child_process", () => ({ ...child, default: child }));

/** A socket whose outcome each test drives by emitting on it. */
class FakeSocket extends EventEmitter {
  destroyed = false;
  timeout: number | undefined;
  setTimeout(ms: number) {
    this.timeout = ms;
  }
  destroy() {
    this.destroyed = true;
  }
}

const net = vi.hoisted(() => ({ connect: vi.fn() }));
vi.mock("node:net", () => ({ default: net, ...net }));

const { ensureDaemonRunning, isExecutable, socketReachable } = await import("./daemon");

let socket: FakeSocket;

beforeEach(() => {
  fs.accessSync.mockReset();
  fs.existsSync.mockReset().mockReturnValue(true);
  child.spawn.mockReset();
  net.connect.mockReset();
  socket = new FakeSocket();
  net.connect.mockImplementation(() => socket);
});

describe("isExecutable", () => {
  it("accepts a file the process may execute", () => {
    fs.accessSync.mockReturnValue(undefined);
    expect(isExecutable("/usr/bin/borgd")).toBe(true);
    expect(fs.accessSync).toHaveBeenCalledWith("/usr/bin/borgd", fs.constants.X_OK);
  });

  it("rejects a file that is missing or not executable", () => {
    // A data file named `borgd` on PATH must not be spawned.
    fs.accessSync.mockImplementation(() => {
      throw new Error("EACCES");
    });
    expect(isExecutable("/usr/bin/borgd")).toBe(false);
  });
});

describe("socketReachable", () => {
  it("does not dial a socket path that does not exist", async () => {
    fs.existsSync.mockReturnValue(false);
    await expect(socketReachable("/nope.sock")).resolves.toBe(false);
    expect(net.connect).not.toHaveBeenCalled();
  });

  it("reports a socket that answers", async () => {
    const reachable = socketReachable("/borgd.sock");
    socket.emit("connect");
    await expect(reachable).resolves.toBe(true);
    expect(socket.destroyed).toBe(true);
  });

  it("reports a stale socket file left by a killed daemon as unreachable", async () => {
    // The whole reason this is a connect and not a stat: starting a second
    // daemon because of a leftover inode is the failure being prevented.
    const reachable = socketReachable("/borgd.sock");
    socket.emit("error", new Error("ECONNREFUSED"));
    await expect(reachable).resolves.toBe(false);
    expect(socket.destroyed).toBe(true);
  });

  it("gives up on a socket that hangs", async () => {
    const reachable = socketReachable("/borgd.sock", 250);
    expect(socket.timeout).toBe(250);
    socket.emit("timeout");
    await expect(reachable).resolves.toBe(false);
  });

  it("detaches from the socket once it has settled", async () => {
    // A probe that stayed attached would keep a handle on every socket it ever
    // dialled, and the poll loop dials one every 250ms.
    const reachable = socketReachable("/borgd.sock");
    socket.emit("connect");
    await expect(reachable).resolves.toBe(true);
    expect(socket.listenerCount("connect")).toBe(0);
    expect(socket.listenerCount("timeout")).toBe(0);
    expect(socket.listenerCount("error")).toBe(0);
  });
});

describe("launching the daemon", () => {
  const deps = {
    appPath: "/repo/apps/desktop",
    platform: "linux" as NodeJS.Platform,
    socketPath: "/home/me/.borg/borgd.sock",
    resolve: () => "/usr/bin/borgd",
    reachable: async () => false,
    waitFor: async () => true,
  };

  it("spawns the daemon detached, so it outlives the window", async () => {
    // Closing a window is not a request to stop being an assistant.
    const unref = vi.fn();
    child.spawn.mockReturnValue({ unref });
    await ensureDaemonRunning({ ...deps, env: { HOME: "/home/me" } }, () => {});

    expect(child.spawn).toHaveBeenCalledWith("/usr/bin/borgd", [], {
      cwd: "/home/me/.borg",
      detached: true,
      stdio: "ignore",
      windowsHide: true,
    });
    expect(unref).toHaveBeenCalledTimes(1);
  });

  it("prefers an explicit BORG_HOME over the one derived from HOME", async () => {
    child.spawn.mockReturnValue({ unref: vi.fn() });
    await ensureDaemonRunning(
      { ...deps, env: { HOME: "/home/me", BORG_HOME: "/data/borg" } },
      () => {},
    );
    expect(child.spawn.mock.calls[0][2]).toMatchObject({ cwd: "/data/borg" });
  });

  it("starts without a working directory when the data directory is not there yet", async () => {
    // First run: `~/.borg` does not exist, and the daemon creates it itself.
    fs.existsSync.mockReturnValue(false);
    child.spawn.mockReturnValue({ unref: vi.fn() });
    await ensureDaemonRunning({ ...deps, env: { HOME: "/home/me" } }, () => {});
    expect(child.spawn.mock.calls[0][2]).toMatchObject({ cwd: undefined });
  });

  it("starts without a working directory when the environment names no home", async () => {
    child.spawn.mockReturnValue({ unref: vi.fn() });
    await ensureDaemonRunning({ ...deps, env: {} }, () => {});
    expect(child.spawn.mock.calls[0][2]).toMatchObject({ cwd: undefined });
  });

  it("probes the candidates itself when no resolver is injected", async () => {
    // The production path: the executable check on disk decides which
    // candidate is spawned.
    fs.accessSync.mockImplementation((file: string) => {
      if (file !== "/custom/borgd") throw new Error("EACCES");
    });
    child.spawn.mockReturnValue({ unref: vi.fn() });
    const { resolve: _omitted, ...withoutResolver } = deps;
    const ok = await ensureDaemonRunning(
      { ...withoutResolver, env: { BORG_DAEMON_BIN: "/custom/borgd" } },
      () => {},
    );
    expect(ok).toBe(true);
    expect(child.spawn).toHaveBeenCalledWith("/custom/borgd", [], expect.anything());
  });

  it("turns a spawn failure into a reported failure rather than a crash", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    child.spawn.mockImplementation(() => {
      throw new Error("EACCES");
    });
    const seen: { state: string }[] = [];
    const ok = await ensureDaemonRunning({ ...deps, env: {} }, (s) => seen.push(s));
    expect(ok).toBe(false);
    expect(seen.at(-1)?.state).toBe("failed");
  });

  it("handles a spawn that throws a non-Error", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    child.spawn.mockImplementation(() => {
      throw "no exec format";
    });
    const seen: { state: string }[] = [];
    await expect(ensureDaemonRunning({ ...deps, env: {} }, (s) => seen.push(s))).resolves.toBe(
      false,
    );
    expect(seen.at(-1)?.state).toBe("failed");
  });
});
