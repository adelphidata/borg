import { describe, expect, it } from "vitest";
import { MODE_KEY, advancedCategoryOf, settingPath } from "./catalog";

describe("settingPath", () => {
  it("sends a key with a curated page there, and every other key to its raw editor", () => {
    expect(settingPath("llm.thinking")).toBe("/settings/thinking");
    expect(settingPath("model")).toBe("/settings/model");
    expect(settingPath(MODE_KEY)).toBe("/settings/mode");
    expect(settingPath("gateway.port")).toBe("/settings/key/gateway.port");
  });
});

describe("advancedCategoryOf", () => {
  it("files meetings keys under Meetings, with its guided page, not under Other", () => {
    const category = advancedCategoryOf("meetings.keep_audio");
    expect(category.name).toBe("Meetings");
    expect(category.page?.path).toBe("/settings/meetings");
  });
});
