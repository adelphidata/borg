import type { HighlighterCore } from "shiki/core";

/**
 * Syntax highlighting for fenced code blocks.
 *
 * Built fine-grained rather than from a prebuilt bundle: Shiki's full bundle
 * carries every grammar and theme it ships and would dominate the app's
 * download. This registers one theme pair and the languages Borg actually
 * emits, and loads the highlighter only when a code block first appears.
 *
 * Shiki's core and regex engine are imported dynamically for the same reason
 * the grammars are: `CodeBlock` sits on a static path from the router, so a
 * top-level import would put ~100 KB of highlighter in the entry chunk and
 * parse it before first paint for every user, including the ones who never
 * scroll past a code block.
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
  // Own properties only. `in` walks the prototype chain, so a fence tagged
  // ```constructor or ```toString would resolve to a "grammar" that is really
  // an Object member, and loading it throws instead of falling back to plain
  // text.
  if (Object.hasOwn(LANGUAGES, key)) return key as keyof typeof LANGUAGES;
  return Object.hasOwn(ALIASES, key) ? ALIASES[key] : null;
}

/**
 * github-dark paints comments #6a737d on #24292e, 3.05:1 — under the 4.5:1
 * text contrast floor. Swap in the theme's own lighter grey (#959da5, 5.3:1);
 * every other token in both themes already clears it.
 */
const DARK_COMMENT = "#6a737d";
const DARK_COMMENT_LEGIBLE = "#959da5";

export function legibleComments<
  T extends { tokenColors?: { settings?: { foreground?: string } }[] },
>(theme: T): T {
  return {
    ...theme,
    tokenColors: theme.tokenColors?.map((t) =>
      t.settings?.foreground?.toLowerCase() === DARK_COMMENT
        ? { ...t, settings: { ...t.settings, foreground: DARK_COMMENT_LEGIBLE } }
        : t,
    ),
  };
}

let highlighter: Promise<HighlighterCore> | undefined;
const loaded = new Set<string>();

function getHighlighter(): Promise<HighlighterCore> {
  highlighter ??= (async () => {
    const [{ createHighlighterCore }, { createJavaScriptRegexEngine }] = await Promise.all([
      import("shiki/core"),
      import("shiki/engine/javascript"),
    ]);
    return createHighlighterCore({
      themes: [
        import("@shikijs/themes/github-light"),
        import("@shikijs/themes/github-dark").then((m) => legibleComments(m.default)),
      ],
      langs: [],
      engine: createJavaScriptRegexEngine(),
    });
  })();
  return highlighter;
}

/**
 * Finished markup, so a block that was highlighted once paints highlighted the
 * next time it mounts. Reopening a conversation remounts every fence in it, and
 * without this each one would show plain text first and recolour a beat later.
 *
 * Bounded by size, not by count. An entry holds the code and its two-theme
 * markup, several times the code's length, and a block that is still streaming
 * adds a new, slightly longer entry on every pass. A count bound lets a few
 * long transcripts hold tens of megabytes.
 *
 * Least recently used out first: a `Map` iterates in insertion order, and a
 * hit moves its key to the end, so the first key is the one used longest ago.
 */
export class HighlightCache {
  private readonly entries = new Map<string, { html: string; size: number }>();
  private total = 0;

  /**
   * `budget` is the most the cache holds, in characters of code plus markup.
   * An entry larger than `entryMax` is never kept: it would push out many
   * small blocks to hold one that is rarely shown twice.
   */
  constructor(
    readonly budget: number,
    readonly entryMax: number,
  ) {}

  /** Characters held now, code plus markup. */
  get size(): number {
    return this.total;
  }

  get(key: string): string | undefined {
    const entry = this.entries.get(key);
    if (!entry) return undefined;
    this.entries.delete(key);
    this.entries.set(key, entry);
    return entry.html;
  }

  set(key: string, code: string, html: string): void {
    const size = code.length + html.length;
    // Two callers can race on one key; the second replaces, not duplicates.
    this.remove(key);
    if (size > this.entryMax) return;
    for (const oldest of this.entries.keys()) {
      if (this.total + size <= this.budget) break;
      this.remove(oldest);
    }
    this.entries.set(key, { html, size });
    this.total += size;
  }

  private remove(key: string): void {
    const entry = this.entries.get(key);
    if (!entry) return;
    this.entries.delete(key);
    this.total -= entry.size;
  }
}

/** About 4 MB of code and markup across every block on screen and recently shown. */
export const HIGHLIGHT_CACHE_BUDGET = 4_000_000;
/** One block's share: a quarter of a megabyte of code and markup. */
export const HIGHLIGHT_ENTRY_MAX = 256_000;
const cache = new HighlightCache(HIGHLIGHT_CACHE_BUDGET, HIGHLIGHT_ENTRY_MAX);

/** A language name holds no NUL, so no two pairs share a key. */
const cacheKey = (lang: string, code: string) => `${lang}\0${code}`;

/**
 * The markup for `code` if it has been highlighted already, without waiting.
 * `undefined` means "not known yet", not "cannot be highlighted".
 */
export function peekHighlight(code: string, tag: string | undefined): string | undefined {
  const lang = resolveLanguage(tag);
  return lang ? cache.get(cacheKey(lang, code)) : undefined;
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
  const key = cacheKey(lang, code);
  const cached = cache.get(key);
  if (cached !== undefined) return cached;

  const shiki = await getHighlighter();
  if (!loaded.has(lang)) {
    await shiki.loadLanguage(await LANGUAGES[lang]());
    loaded.add(lang);
  }

  const html = shiki.codeToHtml(code, {
    lang,
    themes: { light: "github-light", dark: "github-dark" },
    defaultColor: false,
  });
  cache.set(key, code, html);
  return html;
}
