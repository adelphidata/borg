import { afterEach, describe, expect, it, vi } from "vitest";
import { openExternal } from "./openExternal";

afterEach(() => vi.restoreAllMocks());

describe("openExternal", () => {
  it("opens a web page in a separate context the page cannot reach back into", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    expect(openExternal("https://accounts.google.com/o/oauth2/auth?x=1")).toBe(true);
    expect(open).toHaveBeenCalledWith(
      "https://accounts.google.com/o/oauth2/auth?x=1",
      "_blank",
      "noopener,noreferrer",
    );
  });

  it.each(["file:///etc/passwd", "javascript:alert(1)", "not a url"])("refuses %s", (url) => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    expect(openExternal(url)).toBe(false);
    expect(open).not.toHaveBeenCalled();
  });
});
