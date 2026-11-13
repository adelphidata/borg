import { beforeEach, describe, expect, it, vi } from "vitest";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { RECOVERY_RELOAD_URL } from "./recovery";

vi.mock("electron", () => import("./test/electron-stub"));

const { externalOpens, makeWindow, resetElectron } = await import("./test/electron-stub");
const { hardenWebContents } = await import("./harden");

const INDEX_HTML = path.join("/repo/apps/desktop", "dist", "index.html");
const PACKAGED = { isDev: false, indexHtml: INDEX_HTML };
const DEV = { isDev: true, devServerUrl: "http://localhost:5173", indexHtml: INDEX_HTML };

let win: ReturnType<typeof makeWindow>;

/** A window hardened with `options`, as every app window is. */
function harden(options: Parameters<typeof hardenWebContents>[1] = PACKAGED) {
  win = makeWindow();
  hardenWebContents(win as never, options);
}

const openHandler = () => win.webContents.windowOpenHandler!;
const navigate = (url: string) => {
  const event = { preventDefault: vi.fn() };
  win.webContents.emit("will-navigate", event, url);
  return event;
};

beforeEach(() => {
  resetElectron();
});

describe("hardenWebContents: new windows", () => {
  it("never opens an in-app window, and sends the link to the browser", () => {
    harden();
    expect(openHandler()({ url: "https://example.com" })).toEqual({ action: "deny" });
    expect(externalOpens).toEqual(["https://example.com"]);
  });

  it("hands only http and https to the OS", () => {
    // file:, smb:, and custom schemes reaching openExternal is a local-code
    // execution route.
    harden();
    for (const url of [
      "file:///etc/passwd",
      "smb://share/x",
      "borg://run",
      "javascript:alert(1)",
    ]) {
      expect(openHandler()({ url })).toEqual({ action: "deny" });
    }
    expect(externalOpens).toEqual([]);

    openHandler()({ url: "http://example.com" });
    expect(externalOpens).toEqual(["http://example.com"]);
  });

  it("ignores an unparseable URL rather than throwing in the handler", () => {
    harden();
    expect(() => openHandler()({ url: "not a url" })).not.toThrow();
    expect(externalOpens).toEqual([]);
  });
});

describe("hardenWebContents: navigation", () => {
  it("allows navigation within the packaged app", () => {
    harden();
    expect(
      navigate(`${pathToFileURL(INDEX_HTML).href}#/settings`).preventDefault,
    ).not.toHaveBeenCalled();
  });

  it("blocks any other local file, not just a different app", () => {
    // A prefix check would let every file:// path under the app root count as
    // internal.
    harden();
    const event = navigate(
      pathToFileURL(path.join("/repo/apps/desktop", "dist", "evil.html")).href,
    );
    expect(event.preventDefault).toHaveBeenCalled();
    expect(externalOpens).toEqual([]);
  });

  it("blocks an external site and sends it to the browser instead", () => {
    harden();
    expect(navigate("https://evil.example.com").preventDefault).toHaveBeenCalled();
    expect(externalOpens).toEqual(["https://evil.example.com"]);
  });

  it("allows navigation within the dev server's origin", () => {
    harden(DEV);
    expect(navigate("http://localhost:5173/#/usage").preventDefault).not.toHaveBeenCalled();
  });

  it("blocks another port on the same host in dev", () => {
    // localhost:51737 is a different origin, however local it looks.
    harden(DEV);
    expect(navigate("http://localhost:51737/").preventDefault).toHaveBeenCalled();
  });

  it("blocks an unparseable navigation target", () => {
    harden();
    expect(navigate("http://[::").preventDefault).toHaveBeenCalled();
  });

  it("stops the recovery reload link and tells only the window that asked", () => {
    const onRecoveryReload = vi.fn();
    harden({ ...PACKAGED, onRecoveryReload });
    const event = navigate(RECOVERY_RELOAD_URL);
    expect(event.preventDefault).toHaveBeenCalled();
    expect(onRecoveryReload).toHaveBeenCalledTimes(1);
    // A signal, not a destination.
    expect(externalOpens).toEqual([]);
  });

  it("stops the recovery reload link in a window with no recovery page too", () => {
    harden();
    expect(navigate(RECOVERY_RELOAD_URL).preventDefault).toHaveBeenCalled();
    expect(externalOpens).toEqual([]);
  });
});
