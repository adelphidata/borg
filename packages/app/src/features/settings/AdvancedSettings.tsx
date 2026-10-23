import { useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { ChevronIcon, STORAGE_KEYS } from "@borg/ui";
import {
  CategoryCard,
  CategoryFold,
  Chevron,
  FoldBadge,
  ROW_BUTTON,
} from "../../components/CategoryFold";
import type { DaemonSetting } from "../../lib/settings";
import {
  ADVANCED_CATEGORIES,
  OTHER_CATEGORY,
  advancedCategoryOf,
  keyPrefix,
  settingPath,
  type AdvancedCategory,
} from "./catalog";
import { summarizeValue } from "./settingValue";

/**
 * One row of the settings list. The curated rows and the Advanced toggle share
 * it, so Advanced reads as the list's last row rather than a heading of its own.
 */
export const SETTING_ROW = `${ROW_BUTTON} gap-[18px] border-b border-line2 px-0.5 py-4`;
export const SETTING_NAME =
  "min-w-0 flex-1 truncate text-[14px] leading-[1.3] font-medium text-ink";

/** `localeCompare` builds a collator per call; ~500 rows makes that measurable. */
const BY_NAME = new Intl.Collator();

/** One run of keys sharing a prefix; `prefix` is "" for undotted keys. */
interface AdvancedSection {
  prefix: string;
  rows: DaemonSetting[];
}

export interface AdvancedGroup {
  category: AdvancedCategory;
  sections: AdvancedSection[];
  total: number;
  /** Rows overridden in the database — the ones worth a second look. */
  changed: number;
}

/**
 * The registry, minus `skip`, sorted into the Advanced categories in their
 * screen order. Inside a category, keys run by prefix in the order the
 * category lists them, then alphabetically; empty categories drop out.
 */
export function groupAdvanced(
  settings: Iterable<DaemonSetting>,
  skip: ReadonlySet<string>,
): AdvancedGroup[] {
  const byCategory = new Map<string, Map<string, DaemonSetting[]>>();
  for (const entry of settings) {
    if (skip.has(entry.key)) continue;
    const id = advancedCategoryOf(entry.key).id;
    const sections = byCategory.get(id) ?? new Map<string, DaemonSetting[]>();
    const prefix = entry.key.includes(".") ? keyPrefix(entry.key) : "";
    sections.set(prefix, [...(sections.get(prefix) ?? []), entry]);
    byCategory.set(id, sections);
  }
  return [...ADVANCED_CATEGORIES, OTHER_CATEGORY].flatMap((category) => {
    const found = byCategory.get(category.id);
    if (!found) return [];
    const rank = (prefix: string) => {
      const at = category.prefixes.indexOf(prefix);
      return prefix === "" ? -1 : at === -1 ? category.prefixes.length : at;
    };
    const sections = [...found.entries()]
      .sort(([a], [b]) => rank(a) - rank(b) || BY_NAME.compare(a, b))
      .map(([prefix, rows]) => ({
        prefix,
        rows: rows.sort((a, b) => BY_NAME.compare(a.key, b.key)),
      }));
    const rows = sections.flatMap((s) => s.rows);
    return [
      {
        category,
        sections,
        total: rows.length,
        changed: rows.filter((r) => r.overridden).length,
      },
    ];
  });
}

/** What is open under Advanced, kept for the tab so a trip into one setting comes back to it. */
interface FoldState {
  open: boolean;
  categories: string[];
}

function readFolds(): FoldState {
  try {
    const raw = globalThis.sessionStorage?.getItem(STORAGE_KEYS.settingsAdvanced);
    const parsed = raw ? (JSON.parse(raw) as Partial<FoldState>) : {};
    return {
      open: parsed.open === true,
      categories: Array.isArray(parsed.categories) ? parsed.categories : [],
    };
  } catch {
    return { open: false, categories: [] };
  }
}

function writeFolds(state: FoldState): void {
  try {
    globalThis.sessionStorage?.setItem(STORAGE_KEYS.settingsAdvanced, JSON.stringify(state));
  } catch {
    // Losing the folds on the way back is a small cost; failing the click is not.
  }
}

function AdvancedFold({
  group,
  open,
  onToggle,
}: {
  group: AdvancedGroup;
  open: boolean;
  onToggle: () => void;
}) {
  const navigate = useNavigate();
  const { category, sections, total, changed } = group;
  // One run needs no heading: the category already names it.
  const labelled = sections.length > 1;

  return (
    <CategoryFold
      id={`advanced-${category.id}`}
      name={category.name}
      badges={changed > 0 && <FoldBadge>{changed} changed</FoldBadge>}
      total={total}
      open={open}
      onToggle={onToggle}
      bodyClassName="px-1.5 pt-1 pb-1.5"
    >
      {category.page && (
        <button
          type="button"
          className={`${ROW_BUTTON} justify-between gap-[18px] rounded-[7px] px-3 py-[9px] transition-[background] duration-150 hover:bg-surface3`}
          onClick={() => navigate(category.page!.path)}
        >
          <span className="min-w-0 truncate text-[12.5px] leading-[1.3] font-medium text-ink">
            {category.page.name}
          </span>
          <ChevronIcon size={12} className="flex-none text-ink3" />
        </button>
      )}
      {sections.map((section) => (
        <div key={section.prefix || "_"}>
          {labelled && section.prefix !== "" && (
            <div className="px-3 pt-3 pb-1.5 text-[9.5px] leading-none font-medium tracking-[0.16em] uppercase text-ink3">
              {section.prefix.replaceAll("_", " ")}
            </div>
          )}
          {section.rows.map((r) => {
            const shown = summarizeValue(r.key, r.value, r.kind);
            return (
              <button
                type="button"
                key={r.key}
                title={r.key}
                aria-label={`${r.key} ${r.value === "" ? "unset" : shown}`}
                className={`${ROW_BUTTON} justify-between gap-[18px] rounded-[7px] px-3 py-[9px] transition-[background] duration-150 hover:bg-surface3`}
                onClick={() => navigate(settingPath(r.key))}
              >
                <span className="min-w-0 truncate text-[12.5px] leading-[1.3] text-ink2">
                  {section.prefix === "" ? r.key : r.key.slice(section.prefix.length + 1)}
                </span>
                <span
                  className={`max-w-[200px] flex-none truncate text-[12px] leading-none font-medium tabular-nums ${
                    r.overridden ? "text-neon-ink" : "text-ink3"
                  }`}
                >
                  {shown}
                </span>
              </button>
            );
          })}
        </div>
      ))}
    </CategoryFold>
  );
}

/**
 * Everything the curated list does not cover, folded twice: Advanced itself,
 * then one fold per category, so opening it shows a dozen plain-words rows
 * instead of five hundred keys.
 */
export function AdvancedSettings({
  settings,
  skip,
}: {
  settings: Map<string, DaemonSetting>;
  skip: ReadonlySet<string>;
}) {
  const [folds, setFolds] = useState(readFolds);
  // Grouping walks the whole registry (~500 rows) and sorts; once per snapshot.
  const groups = useMemo(() => groupAdvanced(settings.values(), skip), [settings, skip]);

  const update = (next: FoldState) => {
    writeFolds(next);
    setFolds(next);
  };
  const toggleCategory = (id: string) =>
    update({
      ...folds,
      categories: folds.categories.includes(id)
        ? folds.categories.filter((c) => c !== id)
        : [...folds.categories, id],
    });

  return (
    <>
      <button
        type="button"
        aria-expanded={folds.open}
        className={SETTING_ROW}
        onClick={() => update({ ...folds, open: !folds.open })}
      >
        <span className={SETTING_NAME}>Advanced</span>
        <Chevron open={folds.open} />
      </button>

      {folds.open && (
        <div className="pt-4">
          <CategoryCard>
            {groups.map((g) => (
              <AdvancedFold
                key={g.category.id}
                group={g}
                open={folds.categories.includes(g.category.id)}
                onToggle={() => toggleCategory(g.category.id)}
              />
            ))}
          </CategoryCard>
        </div>
      )}
    </>
  );
}
