import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { adoptConversationUrl, usePopCount } from "./conversationUrl";

function fakeWindow(hash: string) {
  const replaceState = vi.fn();
  return {
    win: { location: { hash }, history: { state: { idx: 3 }, replaceState } } as never,
    replaceState,
  };
}

describe("adoptConversationUrl", () => {
  it("swaps /c/new for the session id, keeping the router's history state", () => {
    const { win, replaceState } = fakeWindow("#/c/new");
    adoptConversationUrl("a1b2", win);
    expect(replaceState).toHaveBeenCalledWith({ idx: 3 }, "", "#/c/a1b2");
  });

  it("leaves the address alone once it has moved on, or without an id", () => {
    for (const hash of ["#/c/abc", "#/settings", "#/c/newer", ""]) {
      const { win, replaceState } = fakeWindow(hash);
      adoptConversationUrl("a1b2", win);
      expect(replaceState).not.toHaveBeenCalled();
    }
    const { win, replaceState } = fakeWindow("#/c/new");
    adoptConversationUrl("", win);
    expect(replaceState).not.toHaveBeenCalled();
  });
});

describe("usePopCount", () => {
  it("counts each browser navigation and stops once unmounted", () => {
    const { result, unmount } = renderHook(() => usePopCount());
    expect(result.current).toBe(0);
    act(() => {
      window.dispatchEvent(new PopStateEvent("popstate"));
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    expect(result.current).toBe(2);
    unmount();
    act(() => void window.dispatchEvent(new PopStateEvent("popstate")));
  });
});
