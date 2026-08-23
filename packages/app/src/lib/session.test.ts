import { afterEach, describe, expect, it, vi } from "vitest";
import { STORAGE_KEYS } from "@borg/ui";
import { signOut } from "./session";

afterEach(() => {
  vi.unstubAllGlobals();
  localStorage.clear();
});

describe("signOut", () => {
  it("drops the token and restarts the app", () => {
    // Leaving the token behind would put the user straight back into the app on
    // reload, which is not what signing out means.
    const reload = vi.fn();
    vi.stubGlobal("location", { reload });
    localStorage.setItem(STORAGE_KEYS.token, "tok_live");

    signOut();
    expect(localStorage.getItem(STORAGE_KEYS.token)).toBeNull();
    expect(reload).toHaveBeenCalledTimes(1);
  });
});
