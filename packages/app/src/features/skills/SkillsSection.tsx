import { useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { Pill, PlusIcon, STORAGE_KEYS, TrashIcon } from "@borg/ui";
import type { admin } from "@borg/client";
import { CategoryCard, CategoryFold, FoldBadge } from "../../components/CategoryFold";
import { DaemonState } from "../../components/DaemonState";
import { PageGroup } from "../../components/PageGroup";
import { Switch } from "../../components/Switch";
import { needsNote } from "./SkillSetup";
import { SOURCE_WORDS, useSkills, type Skills } from "./useSkills";

interface SkillCategory {
  id: string;
  name: string;
}

/** Built-in skill categories in screen order: the everyday basics first. */
const SKILL_CATEGORIES: SkillCategory[] = [
  { id: "core", name: "Core" },
  { id: "utilities", name: "Utilities" },
  { id: "payments", name: "Payments" },
  { id: "developer", name: "Developer" },
  { id: "productivity", name: "Productivity" },
];

/** Last: every skill you or a project added, and any built-in no category above takes. */
const CUSTOM: SkillCategory = { id: "custom", name: "Custom" };

function categoryOf(skill: admin.SkillCatalogEntry): SkillCategory {
  if (skill.source !== "builtin") return CUSTOM;
  const id = skill.category.trim().toLowerCase();
  return SKILL_CATEGORIES.find((c) => c.id === id) ?? CUSTOM;
}

interface SkillGroup {
  category: SkillCategory;
  skills: admin.SkillCatalogEntry[];
}

/** The catalog folded by category, in screen order; empty categories drop out. */
function groupSkills(list: admin.SkillCatalogEntry[]): SkillGroup[] {
  return [...SKILL_CATEGORIES, CUSTOM].flatMap((category) => {
    const skills = list.filter((s) => categoryOf(s) === category);
    return skills.length > 0 ? [{ category, skills }] : [];
  });
}

/** The open folds, kept for the tab so a trip into one skill comes back to them. */
function readFolds(): string[] {
  try {
    const parsed: unknown = JSON.parse(
      globalThis.sessionStorage?.getItem(STORAGE_KEYS.skillFolds) ?? "[]",
    );
    return Array.isArray(parsed) ? parsed.filter((id) => typeof id === "string") : [];
  } catch {
    return [];
  }
}

function writeFolds(ids: string[]): void {
  try {
    globalThis.sessionStorage?.setItem(STORAGE_KEYS.skillFolds, JSON.stringify(ids));
  } catch {
    // Losing the folds on the way back is a small cost; failing the click is not.
  }
}

function SkillRow({
  skill,
  onOpen,
  onToggle,
  onDelete,
}: {
  skill: admin.SkillCatalogEntry;
  onOpen: () => void;
  onToggle: (enabled: boolean) => void;
  onDelete?: () => void;
}) {
  const note = skill.mandatory ? "Always on" : skill.enabled ? needsNote(skill.needs) : undefined;
  // The fold already names the category; a tag only marks one that is not built in.
  const source = skill.source === "builtin" ? undefined : SOURCE_WORDS[skill.source];
  return (
    <div className="group flex items-center gap-3 border-b border-line2 py-3 last:border-0">
      <button
        type="button"
        onClick={onOpen}
        className="block min-w-0 flex-1 cursor-pointer border-0 bg-transparent p-0 text-left"
      >
        <div className="flex items-center gap-2">
          <span className="truncate text-[13.5px] leading-[1.35] text-ink">{skill.name}</span>
          {source && (
            <span className="rounded-md border border-line2 px-1.5 py-0.5 text-[9.5px] leading-none tracking-[0.08em] uppercase text-ink3">
              {source}
            </span>
          )}
        </div>
        <div className="mt-[3px] line-clamp-2 text-[11.5px] leading-[1.4] text-ink3">
          {skill.description}
        </div>
        {note && (
          <div
            className={`mt-[3px] text-[11px] leading-[1.4] ${
              skill.mandatory ? "text-ink3" : "text-warn"
            }`}
          >
            {note}
          </div>
        )}
      </button>
      {onDelete && (
        <button
          type="button"
          aria-label={`Delete ${skill.name}`}
          onClick={onDelete}
          className="flex h-7 w-7 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-alert"
        >
          <TrashIcon size={14} />
        </button>
      )}
      <Switch
        on={skill.enabled}
        onChange={onToggle}
        label={`Enable ${skill.name}`}
        disabled={skill.mandatory}
      />
    </div>
  );
}

function SkillFold({
  group,
  open,
  onToggle,
  all,
  confirm,
  setConfirm,
}: {
  group: SkillGroup;
  open: boolean;
  onToggle: () => void;
  all: Skills;
  confirm: string | undefined;
  setConfirm: (name: string | undefined) => void;
}) {
  const navigate = useNavigate();
  const { category, skills } = group;
  const on = skills.filter((s) => s.enabled).length;
  // A folded row hides what it needs, so the fold says how many do.
  const missing = skills.filter((s) => s.enabled && s.needs.length > 0).length;
  return (
    <CategoryFold
      id={`skills-${category.id}`}
      name={category.name}
      badges={
        (missing > 0 || on > 0) && (
          <>
            {missing > 0 && (
              <FoldBadge tone="warn">
                {missing} {missing === 1 ? "needs" : "need"} setup
              </FoldBadge>
            )}
            {on > 0 && <FoldBadge>{on} on</FoldBadge>}
          </>
        )
      }
      total={skills.length}
      open={open}
      onToggle={onToggle}
      bodyClassName="px-4"
    >
      {skills.map((skill) =>
        confirm === skill.name ? (
          <div
            key={skill.name}
            className="my-2 flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3"
          >
            <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
              Delete {skill.name}?
            </div>
            <Pill square onClick={() => setConfirm(undefined)}>
              Keep
            </Pill>
            <Pill
              square
              variant="danger"
              onClick={() => {
                setConfirm(undefined);
                void all.remove(skill.name);
              }}
            >
              Delete
            </Pill>
          </div>
        ) : (
          <SkillRow
            key={skill.name}
            skill={skill}
            onOpen={() => navigate(`/connections/skills/${encodeURIComponent(skill.name)}`)}
            onToggle={(enabled) => void all.setEnabled(skill.name, enabled)}
            onDelete={skill.source === "user" ? () => setConfirm(skill.name) : undefined}
          />
        ),
      )}
    </CategoryFold>
  );
}

/**
 * The skills catalog as a list of switches folded by category the way
 * Advanced settings are: Core first, then the narrower kits, then Custom.
 * One section of the Connections page.
 *
 * A skill of yours sits under Custom with a tag that says so, and is the
 * only kind the list can delete. Built-ins can be turned off, but not
 * edited: the embedded copy always wins, so the detail page offers a copy
 * under a new name instead.
 */
export function SkillsSection() {
  const location = useLocation();
  const navigate = useNavigate();
  const skills = useSkills([location.key]);
  const [confirm, setConfirm] = useState<string>();
  const [open, setOpen] = useState(readFolds);

  const toggle = (id: string) => {
    const next = open.includes(id) ? open.filter((c) => c !== id) : [...open, id];
    writeFolds(next);
    setOpen(next);
  };

  return (
    <PageGroup
      title="Skills"
      action={
        <Pill square onClick={() => navigate("/connections/skills/new")}>
          <span className="flex items-center gap-1.5">
            <PlusIcon size={13} /> New skill
          </span>
        </Pill>
      }
    >
      {skills.error && (
        <div role="alert" className="mb-3 text-[11.5px] leading-[1.4] text-alert">
          {skills.error}
        </div>
      )}

      <DaemonState query={skills.query} label="your skills">
        {(list) => {
          const groups = groupSkills(list);
          return (
            groups.length > 0 && (
              <CategoryCard>
                {groups.map((g) => (
                  <SkillFold
                    key={g.category.id}
                    group={g}
                    open={open.includes(g.category.id)}
                    onToggle={() => toggle(g.category.id)}
                    all={skills}
                    confirm={confirm}
                    setConfirm={setConfirm}
                  />
                ))}
              </CategoryCard>
            )
          );
        }}
      </DaemonState>
    </PageGroup>
  );
}
