import { useEffect, useRef, useState } from "react";
import { useNavigate, useParams, useSearchParams } from "react-router-dom";
import {
  BackLink,
  DETAIL_COL,
  PageChrome,
  Pill,
  SECTION_LABEL,
  TEXT_FIELD,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import type { admin } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { Switch } from "../../components/Switch";
import {
  SOURCE_WORDS,
  frontmatterName,
  skillTemplate,
  useSkill,
  useSkills,
  withFrontmatterName,
} from "./useSkills";

const ERROR_BOX =
  "rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2";
const EDITOR =
  "min-h-[360px] w-full resize-y rounded-[10px] border border-field bg-surface2 px-4 py-3 font-mono text-[12.5px] leading-[1.55] text-ink outline-none select-text placeholder:text-ink3 focus:border-focus";
const LABEL = "mb-1.5 block text-[11.5px] leading-none text-ink3";

/** A fresh user skill; `?from=<name>` starts from a copy of that skill. */
function NewSkill({ from }: { from: string | undefined }) {
  const navigate = useNavigate();
  const skills = useSkills();
  const source = useSkill(from);
  const [name, setName] = useState("");
  const [content, setContent] = useState(() => skillTemplate(""));
  const [touched, setTouched] = useState(false);
  const [localError, setLocalError] = useState<string>();
  const [saving, setSaving] = useState(false);

  // The copy arrives after mount; take it unless something has been typed.
  useEffect(() => {
    if (!from || touched || source.state !== "ready" || !source.data) return;
    setContent(source.data.content);
  }, [from, touched, source]);
  // Keep the frontmatter name in step with the name field until the body
  // has been edited by hand — then it is the author's text.
  useEffect(() => {
    if (touched) return;
    setContent((c) =>
      from && source.state === "ready" && source.data
        ? withFrontmatterName(c, name.trim() || source.data.entry?.name || "")
        : skillTemplate(name),
    );
  }, [name, touched, from, source]);

  const save = () => {
    const id = name.trim();
    // `/skills/new` is this page, so a skill called "new" could never be opened.
    if (!/^[A-Za-z0-9][A-Za-z0-9_-]*$/.test(id) || id === "new") {
      setLocalError("Name it with letters, digits, dashes or underscores, nothing else.");
      return;
    }
    const declared = frontmatterName(content);
    if (declared !== undefined && declared !== id) {
      setLocalError(
        `The file says "name: ${declared}" but the skill is called ${id}. Make them match.`,
      );
      return;
    }
    setLocalError(undefined);
    setSaving(true);
    void skills
      .save(id, content)
      .then((saved) => {
        if (saved) navigate(`/skills/${encodeURIComponent(id)}`, { replace: true });
      })
      .finally(() => setSaving(false));
  };

  return (
    <>
      <h2 className="m-0 text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
        {from ? `Copy of ${from}` : "New skill"}
      </h2>
      <div>
        <label className={LABEL} htmlFor="skill-name">
          Name
        </label>
        <input
          id="skill-name"
          autoFocus
          className={`${TEXT_FIELD} w-full select-text`}
          placeholder="release-notes"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </div>
      <div>
        <label className={LABEL} htmlFor="skill-content">
          SKILL.md
        </label>
        <textarea
          id="skill-content"
          className={EDITOR}
          spellCheck={false}
          value={content}
          onChange={(e) => {
            setTouched(true);
            setContent(e.target.value);
          }}
        />
        <div className="mt-1.5 px-1 text-[11px] leading-[1.4] text-ink3">
          The lines between the dashes describe the skill; the rest is what Borg reads.
        </div>
      </div>
      {(localError || skills.error) && (
        <div role="alert" className={ERROR_BOX}>
          {localError ?? skills.error}
        </div>
      )}
      <div className="flex gap-2">
        <Pill square variant="primary" disabled={saving || name.trim() === ""} onClick={save}>
          {saving ? "Saving…" : "Create skill"}
        </Pill>
        <Pill square onClick={() => navigate("/skills")}>
          Cancel
        </Pill>
      </div>
    </>
  );
}

function Existing({
  detail,
  skills,
}: {
  detail: admin.SkillDetail;
  skills: ReturnType<typeof useSkills>;
}) {
  const navigate = useNavigate();
  const entry = detail.entry!;
  const editable = entry.source === "user";
  const [content, setContent] = useState(detail.content);
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string>();
  const [saving, setSaving] = useState(false);
  const [confirm, setConfirm] = useState(false);

  // Keyed on the content alone: clearing `dirty` after a save must not pull
  // the text that was just replaced back into the editor while the detail
  // is still being re-read.
  const dirtyRef = useRef(false);
  const markDirty = (next: boolean) => {
    dirtyRef.current = next;
    setDirty(next);
  };
  useEffect(() => {
    if (!dirtyRef.current) setContent(detail.content);
  }, [detail.content]);

  const save = () => {
    const declared = frontmatterName(content);
    if (declared !== undefined && declared !== entry.name) {
      setLocalError(
        `The file says "name: ${declared}" but this skill is ${entry.name}. Make them match.`,
      );
      return;
    }
    setLocalError(undefined);
    setSaving(true);
    void skills
      .save(entry.name, content)
      .then((saved) => {
        if (!saved) return;
        setContent(saved.content);
        markDirty(false);
      })
      .finally(() => setSaving(false));
  };

  return (
    <>
      <div className="flex items-start gap-4">
        <div className="min-w-0 flex-1">
          <h2 className="m-0 truncate text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
            {entry.name}
          </h2>
          <div className="mt-1 text-[12.5px] leading-[1.45] text-ink3">{entry.description}</div>
          <div className="mt-1.5 text-[11.5px] leading-[1.4] text-ink3">
            {SOURCE_WORDS[entry.source] ?? entry.source}
            {entry.category && <> · {entry.category}</>}
            {entry.mandatory && <> · always on</>}
            {!entry.mandatory && entry.enabled && !entry.available && (
              <span className="text-alert"> · something it needs is missing</span>
            )}
          </div>
        </div>
        <Switch
          on={entry.enabled}
          onChange={(next) => void skills.setEnabled(entry.name, next)}
          label={`Enable ${entry.name}`}
          disabled={entry.mandatory}
        />
      </div>

      <section>
        <div className={SECTION_LABEL}>SKILL.md</div>
        {editable ? (
          <>
            <textarea
              aria-label="SKILL.md"
              className={EDITOR}
              spellCheck={false}
              value={content}
              onChange={(e) => {
                setContent(e.target.value);
                markDirty(true);
              }}
            />
            {detail.path && (
              <div className="mt-1.5 px-1 font-mono text-[11px] leading-[1.4] text-ink3 select-text">
                {detail.path}
              </div>
            )}
            <div className="mt-3 flex items-center gap-2">
              <Pill square variant="primary" disabled={!dirty || saving} onClick={save}>
                {saving ? "Saving…" : "Save changes"}
              </Pill>
              {dirty && (
                <Pill
                  square
                  onClick={() => {
                    setContent(detail.content);
                    markDirty(false);
                    setLocalError(undefined);
                    skills.resetError();
                  }}
                >
                  Discard
                </Pill>
              )}
            </div>
          </>
        ) : (
          <>
            <pre className="m-0 max-h-[480px] overflow-auto rounded-[10px] border border-line2 bg-surface2 px-4 py-3 font-mono text-[12px] leading-[1.55] whitespace-pre-wrap text-ink2 select-text">
              {detail.content}
            </pre>
            <div className="mt-3 flex flex-wrap items-center gap-3">
              <Pill
                square
                onClick={() => navigate(`/skills/new?from=${encodeURIComponent(entry.name)}`)}
              >
                Copy to a new skill
              </Pill>
              <span className="text-[11.5px] leading-[1.4] text-ink3">
                {entry.source === "builtin"
                  ? "Built-in skills cannot be edited in place; a copy under a new name can."
                  : "This skill lives in the project checkout; edit it there."}
              </span>
            </div>
          </>
        )}
        {(localError || skills.error) && (
          <div role="alert" className={`mt-3 ${ERROR_BOX}`}>
            {localError ?? skills.error}
          </div>
        )}
      </section>

      {editable && (
        <section>
          <div className={SECTION_LABEL}>Delete</div>
          {!confirm ? (
            <Pill square variant="danger" onClick={() => setConfirm(true)}>
              Delete this skill
            </Pill>
          ) : (
            <div className="flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3">
              <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
                Delete {entry.name}? Its folder and everything in it goes.
              </div>
              <Pill square onClick={() => setConfirm(false)}>
                Keep
              </Pill>
              <Pill
                square
                variant="danger"
                onClick={() =>
                  void skills.remove(entry.name).then((r) => r !== undefined && navigate("/skills"))
                }
              >
                Delete
              </Pill>
            </div>
          )}
        </section>
      )}
    </>
  );
}

function ExistingSkill({ name }: { name: string }) {
  const detail = useSkill(name);
  const skills = useSkills();
  // A toggle or save on this page must show up here, not only in the list.
  const { refetch } = detail;
  const listVersion = skills.query.state === "ready" ? skills.query.data : undefined;
  useEffect(() => {
    if (listVersion) refetch();
  }, [listVersion, refetch]);
  return (
    <DaemonState query={detail} label="this skill">
      {(d) => (d?.entry ? <Existing detail={d} skills={skills} /> : null)}
    </DaemonState>
  );
}

/** `/skills/new` creates a user skill; `/skills/:name` shows or edits one. */
export function SkillDetailPage() {
  const isMobile = useIsMobile();
  const { name = "" } = useParams();
  const [params] = useSearchParams();
  const isNew = name === "new";

  return (
    <PageChrome title="Skills">
      <div className={scrollCol(isMobile, "px-[38px] py-[34px]")}>
        <div className={DETAIL_COL}>
          <BackLink to="/skills" label="Skills" />
          {isNew ? (
            <NewSkill from={params.get("from") ?? undefined} />
          ) : (
            <ExistingSkill name={name} />
          )}
        </div>
      </div>
    </PageChrome>
  );
}
