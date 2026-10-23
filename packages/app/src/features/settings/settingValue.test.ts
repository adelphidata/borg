import { describe, expect, it } from "vitest";
import {
  compactJson,
  groupDigits,
  prettyJson,
  summarizeValue,
  ungroupDigits,
  valueShape,
} from "./settingValue";

describe("valueShape", () => {
  it.each([
    ["security.blocked_paths", '[".ssh",".aws"]', "json"],
    ["security.action_limits", '{"tool_calls_warn":200}', "json"],
    ["llm.request_timeout_ms", "120000", "integer"],
    ["budget.monthly_token_limit", "-1", "integer"],
    ["llm.model", "claude-opus-5", "text"],
    // Brackets that do not parse are text, not a broken list.
    ["user.note", "[draft", "text"],
    // A JSON scalar is just a value; only lists and objects get the JSON box.
    ["some.flag", "true", "text"],
  ] as const)("reads %s = %j as %s", (key, value, kind) => {
    expect(valueShape(key, value).kind).toBe(kind);
  });

  it.each(["gateway.port", "apns.port", "telemetry.pid", "update.version", "user.birth_year"])(
    "keeps %s as text, because it names a thing rather than counting one",
    (key) => {
      expect(valueShape(key, "65535").kind).toBe("text");
    },
  );
});

describe("groupDigits and ungroupDigits", () => {
  it.each([
    ["120000", "120,000"],
    ["1000000", "1,000,000"],
    ["999", "999"],
    ["-4500", "-4,500"],
    ["claude-opus-5", "claude-opus-5"],
  ])("groups %s as %s", (raw, shown) => {
    expect(groupDigits(raw)).toBe(shown);
  });

  it("round-trips what it grouped, including a comma the user typed elsewhere", () => {
    expect(ungroupDigits("1,000,000")).toBe("1000000");
    expect(ungroupDigits(" 2,500 ")).toBe("2500");
    expect(ungroupDigits("12000")).toBe("12000");
  });

  it("leaves a comma that belongs to real text alone", () => {
    expect(ungroupDigits("a,b")).toBe("a,b");
    expect(ungroupDigits(",")).toBe(",");
  });
});

describe("JSON editing", () => {
  it("pretty-prints for the editor and compacts for the daemon", () => {
    const pretty = prettyJson(["a", "b"]);
    expect(pretty).toBe('[\n  "a",\n  "b"\n]');
    expect(compactJson(pretty)).toEqual({ value: '["a","b"]' });
  });

  it("refuses a draft that does not parse, in words", () => {
    const out = compactJson('[".ssh",');
    expect("error" in out && out.error).toMatch(/not valid JSON/);
  });
});

describe("summarizeValue", () => {
  it.each([
    ["llm.request_timeout_ms", "120000", "120,000"],
    ["gateway.port", "7842", "7842"],
    ["security.blocked_paths", '[".ssh",".aws",".gnupg"]', "3 items"],
    ["one.list", '["x"]', "1 item"],
    ["security.action_limits", '{"a":1,"b":2}', "2 entries"],
    ["one.map", '{"a":1}', "1 entry"],
    ["empty.list", "[]", "None"],
    ["empty.map", "{}", "None"],
    ["unset.key", "", "—"],
    ["llm.model", "claude-opus-5", "claude-opus-5"],
  ])("shows %s = %j as %j", (key, value, shown) => {
    expect(summarizeValue(key, value)).toBe(shown);
  });
});
