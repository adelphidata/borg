import type { admin } from "@borg/client";
import { useMutation, useQuery, type QueryResult } from "../../lib/useDaemon";

/** Where a skill came from, in words. */
export const SOURCE_WORDS: Record<string, string> = {
  builtin: "Built in",
  user: "Yours",
  project: "From this project",
};

/** A starting point for a new skill: the frontmatter the loader requires. */
export function skillTemplate(name: string): string {
  const title = name.trim() || "my-skill";
  return `---
name: ${title}
description: One line on when Borg should reach for this.
---

# ${title}

Write the steps Borg should follow here. Be concrete: which tools to use,
what to check first, and what a finished result looks like.
`;
}

/** The frontmatter block of a SKILL.md as `[start, end)` offsets, if present. */
function frontmatterSpan(content: string): [number, number] | undefined {
  const lead = content.length - content.trimStart().length;
  if (!content.startsWith("---", lead)) return undefined;
  const end = content.indexOf("\n---", lead + 3);
  if (end < 0) return undefined;
  return [lead + 3, end];
}

/**
 * The same file with `name:` in the frontmatter set to `name`. Only the
 * frontmatter is touched: a `name:` in the body, say inside a code block,
 * is the author's text.
 */
export function withFrontmatterName(content: string, name: string): string {
  const span = frontmatterSpan(content);
  if (!span) return content;
  const [start, end] = span;
  const head = content.slice(start, end);
  const replaced = /^name:.*$/m.test(head)
    ? head.replace(/^name:.*$/m, `name: ${name}`)
    : `\nname: ${name}${head}`;
  return content.slice(0, start) + replaced + content.slice(end);
}

/** The name declared in a SKILL.md's frontmatter, if there is one. */
export function frontmatterName(content: string): string | undefined {
  const span = frontmatterSpan(content);
  if (!span) return undefined;
  const match = /^name:\s*["']?([^"'\n]+?)["']?\s*$/m.exec(content.slice(span[0], span[1]));
  return match?.[1]?.trim();
}

export interface Skills {
  query: QueryResult<admin.SkillCatalogEntry[]>;
  setEnabled: (name: string, enabled: boolean) => Promise<unknown>;
  save: (name: string, content: string) => Promise<admin.SkillDetail | undefined>;
  remove: (name: string) => Promise<unknown>;
  error: string | undefined;
  resetError: () => void;
}

/**
 * Skills — the playbooks Borg reads before a task.
 *
 * The catalog is the loader's full view, disabled rows included, so the
 * switch can be offered; built-ins can be switched but not edited.
 */
export function useSkills(deps: unknown[] = []): Skills {
  const query = useQuery(async (borg) => (await borg.admin.listSkillCatalog({})).skills, deps);
  const refresh = { onSuccess: () => query.refetch() };

  const toggle = useMutation(
    (borg, vars: { name: string; enabled: boolean }) => borg.admin.setSkillEnabled(vars),
    refresh,
  );
  const put = useMutation(
    (borg, vars: { name: string; content: string }) => borg.admin.putSkill(vars),
    refresh,
  );
  const del = useMutation((borg, name: string) => borg.admin.deleteSkill({ name }), refresh);

  return {
    query,
    setEnabled: (name, enabled) => toggle.run({ name, enabled }),
    save: (name, content) => put.run({ name, content }),
    remove: (name) => del.run(name),
    error: (toggle.error ?? put.error ?? del.error)?.message,
    resetError: () => {
      toggle.reset();
      put.reset();
      del.reset();
    },
  };
}

/**
 * One skill with its SKILL.md, re-read whenever `name` changes. With no
 * name there is nothing to ask for, and the query settles on `undefined`
 * rather than sending an empty lookup.
 */
export function useSkill(name: string | undefined): QueryResult<admin.SkillDetail | undefined> {
  return useQuery(
    (borg) => (name ? borg.admin.getSkill({ name }) : Promise.resolve(undefined)),
    [name],
  );
}
