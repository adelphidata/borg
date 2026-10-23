import { useState } from "react";
import { CodeBlock, Pill, SECTION_LABEL, TEXT_FIELD } from "@borg/ui";
import type { admin } from "@borg/client";
import { useMutation } from "../../lib/useDaemon";

const SYSTEM_NAMES: Record<string, string> = {
  macos: "macOS",
  darwin: "macOS",
  linux: "Linux",
  windows: "Windows",
};

const CARD = "rounded-[10px] border border-line2 bg-surface2 px-4 py-3";
const NAME = "min-w-0 flex-1 truncate font-mono text-[12.5px] leading-[1.4] text-ink";
const NOTE = "text-[12px] leading-[1.45] text-ink3";

/** "a", "a or b", "a, b or c". */
function listOf(items: string[], last: "and" | "or"): string {
  if (items.length <= 1) return items[0] ?? "";
  return `${items.slice(0, -1).join(", ")} ${last} ${items[items.length - 1]}`;
}

const systems = (need: admin.SkillNeed) =>
  listOf(
    need.options.map((o) => SYSTEM_NAMES[o.name] ?? o.name),
    "or",
  );

/**
 * What a skill still needs, in one line for its row: the tools and keys it
 * waits on, or the systems it runs on when this computer is not one of them.
 */
export function needsNote(needs: admin.SkillNeed[]): string | undefined {
  const os = needs.find((n) => n.kind === "os");
  if (os) return `Runs only on ${systems(os)}`;
  if (needs.length === 0) return undefined;
  return `Needs ${listOf(
    needs.map((n) =>
      listOf(
        n.options.map((o) => o.name),
        "or",
      ),
    ),
    "and",
  )}`;
}

function ToolOption({
  option,
  installing,
  busy,
  onInstall,
}: {
  option: admin.SkillNeedOption;
  installing: boolean;
  busy: boolean;
  onInstall: () => void;
}) {
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex items-center gap-3">
        <span className={NAME}>{option.name}</span>
        {option.url && !option.installable && (
          <a
            href={option.url}
            target="_blank"
            rel="noreferrer"
            className="flex-none text-[12px] leading-none text-neon-ink"
          >
            How to install
          </a>
        )}
        {option.installable && (
          <Pill square variant="primary" disabled={busy} onClick={onInstall}>
            {installing ? "Installing…" : "Install"}
          </Pill>
        )}
      </div>
      {installing && <div className={NOTE}>This can take a few minutes.</div>}
      {!option.installable && option.install && <CodeBlock code={option.install} lang="sh" />}
      {!option.installable && !option.install && !option.url && (
        <div className={NOTE}>Install {option.name}, then check again.</div>
      )}
    </div>
  );
}

function SecretField({
  name,
  busy,
  onSave,
}: {
  name: string;
  busy: boolean;
  onSave: (value: string) => Promise<unknown>;
}) {
  const [value, setValue] = useState("");
  const save = () => {
    if (!value.trim() || busy) return;
    void onSave(value).then((saved) => saved !== undefined && setValue(""));
  };
  return (
    <div className="flex flex-col gap-2">
      <label className={NAME} htmlFor={`secret-${name}`}>
        {name}
      </label>
      <div className="flex gap-2">
        <input
          id={`secret-${name}`}
          type="password"
          autoComplete="off"
          className={`${TEXT_FIELD} min-w-0 flex-1 select-text`}
          placeholder="Paste the value"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && save()}
        />
        <Pill square variant="primary" disabled={busy || !value.trim()} onClick={save}>
          Save
        </Pill>
      </div>
    </div>
  );
}

/**
 * Each thing a skill still needs on this machine, with the way to meet it
 * right here: an Install button where Borg may run the package manager
 * itself, the command to run where it may not, and a field for a key.
 */
export function SkillSetup({
  entry,
  onChange,
}: {
  entry: admin.SkillCatalogEntry;
  /** Re-read the skill: after a fix, and on "Check again". */
  onChange: () => void;
}) {
  const [installing, setInstalling] = useState<string>();
  const install = useMutation(
    (borg, tool: string) => borg.admin.installSkillTool({ skill: entry.name, tool }),
    { onSuccess: onChange },
  );
  const secret = useMutation(
    (borg, v: { name: string; value: string }) =>
      borg.admin.setSkillSecret({ skill: entry.name, name: v.name, value: v.value }),
    { onSuccess: onChange },
  );
  const busy = install.pending || secret.pending;
  const error = install.error ?? secret.error;

  return (
    <section>
      <div className={SECTION_LABEL}>Set up</div>
      <div className="flex flex-col gap-2.5">
        {entry.needs.map((need) => {
          const key = `${need.kind}:${need.options.map((o) => o.name).join("|")}`;
          if (need.kind === "os") {
            return (
              <div key={key} className={`${CARD} text-[12.5px] leading-[1.45] text-ink2`}>
                Runs only on {systems(need)}. It cannot run on this computer.
              </div>
            );
          }
          if (need.kind === "env") {
            const name = need.options[0]?.name ?? "";
            return (
              <div key={key} className={CARD}>
                <SecretField
                  name={name}
                  busy={busy}
                  onSave={(value) => secret.run({ name, value })}
                />
              </div>
            );
          }
          return (
            <div key={key} className={`${CARD} flex flex-col gap-3`}>
              {need.options.length > 1 && <div className={NOTE}>Any one of these:</div>}
              {need.options.map((option) => (
                <ToolOption
                  key={option.name}
                  option={option}
                  busy={busy}
                  installing={install.pending && installing === option.name}
                  onInstall={() => {
                    setInstalling(option.name);
                    void install.run(option.name);
                  }}
                />
              ))}
            </div>
          );
        })}
      </div>
      {error && (
        <div
          role="alert"
          className="mt-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] whitespace-pre-wrap text-ink2"
        >
          {error.message}
        </div>
      )}
      <div className="mt-3">
        <Pill square disabled={busy} onClick={onChange}>
          Check again
        </Pill>
      </div>
    </section>
  );
}
