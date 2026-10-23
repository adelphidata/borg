import { useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { Pill, PlusIcon, SECTION_LABEL, TrashIcon } from "@borg/ui";
import type { admin } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { Switch } from "../../components/Switch";
import { useSkills, type Skills } from "./useSkills";

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
  const note = skill.mandatory
    ? "Always on"
    : skill.enabled && !skill.available
      ? "On, but something it needs is missing"
      : undefined;
  return (
    <div className="group flex items-center gap-3 border-b border-line2 py-3 last:border-0">
      <button
        type="button"
        onClick={onOpen}
        className="block min-w-0 flex-1 cursor-pointer border-0 bg-transparent p-0 text-left"
      >
        <div className="flex items-center gap-2">
          <span className="truncate text-[13.5px] leading-[1.35] text-ink">{skill.name}</span>
          {skill.category && (
            <span className="rounded-md border border-line2 px-1.5 py-0.5 text-[9.5px] leading-none tracking-[0.08em] uppercase text-ink3">
              {skill.category}
            </span>
          )}
        </div>
        <div className="mt-[3px] line-clamp-2 text-[11.5px] leading-[1.4] text-ink3">
          {skill.description}
        </div>
        {note && (
          <div
            className={`mt-[3px] text-[11px] leading-[1.4] ${
              skill.mandatory ? "text-ink3" : "text-alert"
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

function Group({
  label,
  skills,
  mine,
  all,
  confirm,
  setConfirm,
}: {
  label: string;
  skills: admin.SkillCatalogEntry[];
  mine: boolean;
  all: Skills;
  confirm: string | undefined;
  setConfirm: (name: string | undefined) => void;
}) {
  const navigate = useNavigate();
  if (skills.length === 0) return null;
  return (
    <div className="mb-5">
      <div className="mb-1 text-[11.5px] leading-[1.4] text-ink3">{label}</div>
      <div className="flex flex-col">
        {skills.map((skill) =>
          confirm === skill.name ? (
            <div
              key={skill.name}
              className="my-1 flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3"
            >
              <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
                Delete {skill.name}? Its folder and everything in it goes.
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
              onDelete={mine && skill.source === "user" ? () => setConfirm(skill.name) : undefined}
            />
          ),
        )}
      </div>
    </div>
  );
}

/**
 * Skills — what Borg knows how to do, as a list of switches. One section of
 * the Connections page.
 *
 * Yours come first because they are the ones you can change. Built-ins can
 * be turned off, but not edited: the embedded copy always wins, so the
 * detail page offers a copy under a new name instead.
 */
export function SkillsSection() {
  const location = useLocation();
  const navigate = useNavigate();
  const skills = useSkills([location.key]);
  const [confirm, setConfirm] = useState<string>();

  const on = skills.query.state === "ready" ? skills.query.data.filter((s) => s.enabled).length : 0;

  return (
    <section>
      <div className="mb-3 flex items-center justify-between gap-3">
        <div className={`${SECTION_LABEL} mb-0`}>
          Skills
          {skills.query.state === "ready" && (
            <span className="ml-2 font-normal normal-case tracking-normal text-ink3">
              {on} of {skills.query.data.length} on
            </span>
          )}
        </div>
        <Pill square onClick={() => navigate("/connections/skills/new")}>
          <span className="flex items-center gap-1.5">
            <PlusIcon size={13} /> New skill
          </span>
        </Pill>
      </div>

      {skills.error && (
        <div role="alert" className="mb-3 text-[11.5px] leading-[1.4] text-alert">
          {skills.error}
        </div>
      )}

      <DaemonState query={skills.query} label="your skills">
        {(list) => {
          const mine = list.filter((s) => s.source !== "builtin");
          const builtin = list.filter((s) => s.source === "builtin");
          return (
            <>
              {mine.length === 0 && (
                <div className="mb-5 rounded-[10px] border border-line border-dashed px-4 py-5 text-center text-[12.5px] leading-[1.6] text-ink3">
                  You have not written a skill yet. A skill is a short playbook Borg reads before a
                  task it matches.
                </div>
              )}
              <Group
                label="Your skills"
                skills={mine}
                mine
                all={skills}
                confirm={confirm}
                setConfirm={setConfirm}
              />
              <Group
                label="Built in"
                skills={builtin}
                mine={false}
                all={skills}
                confirm={confirm}
                setConfirm={setConfirm}
              />
            </>
          );
        }}
      </DaemonState>
    </section>
  );
}
