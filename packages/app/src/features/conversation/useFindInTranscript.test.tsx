import { describe, expect, it } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useFindInTranscript } from "./useFindInTranscript";

function mountFind() {
  const scope = { current: document.createElement("div") };
  return renderHook(() => useFindInTranscript(scope, 0));
}

describe("useFindInTranscript focus requests", () => {
  it("asks for focus again when the shortcut is pressed on an open bar", () => {
    const { result } = mountFind();
    act(() => result.current.show());
    expect(result.current.open).toBe(true);
    const first = result.current.focusRequest;

    act(() => result.current.show());

    // Opening twice changes nothing, so the request is the only signal.
    expect(result.current.open).toBe(true);
    expect(result.current.focusRequest).not.toBe(first);
  });

  it("asks for nothing when the query or the position changes", () => {
    const { result } = mountFind();
    act(() => result.current.show());
    const request = result.current.focusRequest;

    act(() => result.current.setQuery("borg"));
    act(() => result.current.next());

    expect(result.current.focusRequest).toBe(request);
  });
});
