import { createHighlighterCore, type HighlighterCore } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";

/**
 * Syntax highlighting for fenced code blocks.
 *
 * Built fine-grained rather than from a prebuilt bundle: Shiki's full bundle
 * carries every grammar and theme it ships and would dominate the app's
 * download. This registers one theme pair and the languages Borg actually
 * emits, and loads the highlighter only when a code block first appears.
 *
 * The JavaScript regex engine replaces the default Oniguruma one, which needs
 * a WASM payload. It cannot compile a handful of exotic grammars; none of the
 * languages below are affected.
 */

/** Languages worth loading. Anything else renders as plain text. */
const LANGUAGES = {
  bash: () => import("@shikijs/langs/bash"),
  c: () => import("@shikijs/langs/c"),
  cpp: () => import("@shikijs/langs/cpp"),
  css: () => import("@shikijs/langs/css"),
  diff: () => import("@shikijs/langs/diff"),
  go: () => import("@shikijs/langs/go"),
  html: () => import("@shikijs/langs/html"),
  java: () => import("@shikijs/langs/java"),
  javascript: () => import("@shikijs/langs/javascript"),
  json: () => import("@shikijs/langs/json"),
  jsx: () => import("@shikijs/langs/jsx"),
  markdown: () => import("@shikijs/langs/markdown"),
  python: () => import("@shikijs/langs/python"),
  ruby: () => import("@shikijs/langs/ruby"),
  rust: () => import("@shikijs/langs/rust"),
  sql: () => import("@shikijs/langs/sql"),
  toml: () => import("@shikijs/langs/toml"),
  tsx: () => import("@shikijs/langs/tsx"),
  typescript: () => import("@shikijs/langs/typescript"),
  yaml: () => import("@shikijs/langs/yaml"),
} as const;

/** Fence tags people actually type, mapped onto the grammars above. */
const ALIASES: Record<string, keyof typeof LANGUAGES> = {
  sh: "bash",
  shell: "bash",
  zsh: "bash",
  console: "bash",
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  ts: "typescript",
  py: "python",
  rb: "ruby",
  rs: "rust",
  yml: "yaml",
  md: "markdown",
  "c++": "cpp",
  patch: "diff",
};

/** Resolves a fence tag to a loadable grammar, or null when unsupported. */
export function resolveLanguage(tag: string | undefined): keyof typeof LANGUAGES | null {
  if (!tag) return null;
  const key = tag.toLowerCase();
  if (key in LANGUAGES) return key as keyof typeof LANGUAGES;
  return ALIASES[key] ?? null;
}

let highlighter: Promise<HighlighterCore> | undefined;
const loaded = new Set<string>();

function getHighlighter(): Promise<HighlighterCore> {
  highlighter ??= createHighlighterCore({
    themes: [import("@shikijs/themes/github-light"), import("@shikijs/themes/github-dark")],
    langs: [],
    engine: createJavaScriptRegexEngine(),
  });
  return highlighter;
}

/**
 * Highlights `code` and returns Shiki's HTML.
 *
 * Both themes are baked into the same markup as `--shiki-light` /
 * `--shiki-dark` custom properties, so switching theme is a CSS swap rather
 * than a re-highlight — see the `.shiki` rules in `styles/app.css`.
 */
export async function highlight(code: string, tag: string | undefined): Promise<string | null> {
  const lang = resolveLanguage(tag);
  if (!lang) return null;

  const shiki = await getHighlighter();
  if (!loaded.has(lang)) {
    await shiki.loadLanguage(await LANGUAGES[lang]());
    loaded.add(lang);
  }

  return shiki.codeToHtml(code, {
    lang,
    themes: { light: "github-light", dark: "github-dark" },
    defaultColor: false,
  });
}
