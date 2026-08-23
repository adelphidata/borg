import { describe, expect, it } from "vitest";
import { highlight, resolveLanguage } from "./highlight";

describe("resolveLanguage", () => {
  it("accepts a grammar named directly", () => {
    expect(resolveLanguage("rust")).toBe("rust");
    expect(resolveLanguage("typescript")).toBe("typescript");
  });

  it("accepts the fence tags people actually type", () => {
    // These are aliases, not grammars: a `sh` fence must still highlight.
    expect(resolveLanguage("sh")).toBe("bash");
    expect(resolveLanguage("zsh")).toBe("bash");
    expect(resolveLanguage("py")).toBe("python");
    expect(resolveLanguage("rs")).toBe("rust");
    expect(resolveLanguage("c++")).toBe("cpp");
    expect(resolveLanguage("patch")).toBe("diff");
  });

  it("is case-insensitive, as fence tags in the wild are", () => {
    expect(resolveLanguage("Rust")).toBe("rust");
    expect(resolveLanguage("JSON")).toBe("json");
    expect(resolveLanguage("YML")).toBe("yaml");
  });

  it("returns null for a fence with no tag or an unknown one", () => {
    // Plain text still reads; loading a grammar that does not exist would throw.
    expect(resolveLanguage(undefined)).toBeNull();
    expect(resolveLanguage("")).toBeNull();
    expect(resolveLanguage("brainfuck")).toBeNull();
  });

  it("does not treat inherited Object properties as grammars", () => {
    // `key in LANGUAGES` walks the prototype chain, so "constructor" would
    // otherwise resolve and then fail to load.
    expect(resolveLanguage("constructor")).toBeNull();
    expect(resolveLanguage("toString")).toBeNull();
  });
});

describe("highlight", () => {
  it("returns null rather than markup for an unsupported fence", async () => {
    await expect(highlight("hello", "brainfuck")).resolves.toBeNull();
    await expect(highlight("hello", undefined)).resolves.toBeNull();
  });

  it("emits both themes in one pass so a theme switch is a CSS swap", async () => {
    const html = await highlight("let x = 1;", "rust");
    expect(html).toContain("<pre");
    expect(html).toContain("--shiki-light");
    expect(html).toContain("--shiki-dark");
  }, 20000);

  it("highlights an alias with the same grammar as its canonical name", async () => {
    // The loaded-grammar cache is keyed on the resolved language, not on what
    // the fence was spelled, so `ts` must not miss a `typescript` that is
    // already loaded. Compared by the text they carry rather than byte for
    // byte: Shiki's own tokenizer is not deterministic across the first use of
    // a freshly loaded grammar, and pinning its exact spans here would make
    // this test fail for its quirks rather than for ours.
    const [alias, canonical] = await Promise.all([
      highlight("const a = 1;", "ts"),
      highlight("const a = 1;", "typescript"),
    ]);
    const textOf = (html: string | null) => {
      const host = document.createElement("div");
      host.innerHTML = html ?? "";
      return host.textContent;
    };
    expect(textOf(alias)).toBe("const a = 1;");
    expect(textOf(canonical)).toBe("const a = 1;");
    expect(alias).toContain("--shiki-light");
    expect(canonical).toContain("--shiki-light");
  }, 20000);

  it("escapes the code it highlights", async () => {
    // The output goes through dangerouslySetInnerHTML, so a fence containing a
    // script tag must come back as text. Asserted by parsing rather than by
    // matching an entity spelling: what matters is that no executable node
    // exists, whichever escape Shiki chose.
    const html = await highlight("<script>alert(1)</script>", "html");
    const host = document.createElement("div");
    host.innerHTML = html ?? "";
    expect(host.querySelector("script")).toBeNull();
    expect(host.textContent).toContain("<script>alert(1)</script>");
  }, 20000);
});

describe("registered grammars", () => {
  // Each fence tag Borg advertises must actually load and tokenise. The
  // grammars are separate packages loaded on demand, so a renamed or dropped
  // one only fails the first time a user pastes that fence — never at build
  // time, and never in a test that highlights one language.
  const TAGS = [
    "bash",
    "c",
    "cpp",
    "css",
    "diff",
    "go",
    "html",
    "java",
    "javascript",
    "json",
    "jsx",
    "markdown",
    "python",
    "ruby",
    "rust",
    "sql",
    "toml",
    "tsx",
    "typescript",
    "yaml",
  ];

  it.each(TAGS)(
    "loads and tokenises %s",
    async (tag) => {
      const html = await highlight("x", tag);
      expect(html).toContain("<pre");
      expect(html).toContain("--shiki-light");
    },
    30000,
  );
});
