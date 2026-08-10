import { useState } from "react";
import { CodeBlock } from "@borg/ui";
import type { FileEdit } from "../turnEdits";
import { KICKER } from "./styles";

/**
 * What the turn changed on disk.
 *
 * A turn that edits a dozen files says so across a dozen tool rows, none of
 * which answers the question the user actually has when it stops: what is
 * different now? This closes the turn with that answer — the files, the size of
 * each change, and the diff one click away.
 */
export function ChangedFilesCard({
  edits,
  uncounted = 0,
}: {
  edits: FileEdit[];
  /** Patches whose output was too long to arrive whole, so cannot be counted. */
  uncounted?: number;
}) {
  const [openPath, setOpenPath] = useState<string>();

  return (
    <div className="rounded-xl border border-line2">
      <div className={`px-[18px] pt-4 ${KICKER}`}>
        {edits.length} file{edits.length === 1 ? "" : "s"} changed
      </div>
      <div className="pb-2">
        {edits.map((edit) => {
          const open = openPath === edit.path;
          return (
            <div key={edit.path}>
              <button
                type="button"
                aria-expanded={open}
                onClick={() => setOpenPath(open ? undefined : edit.path)}
                className="flex w-full cursor-pointer items-center gap-3 border-0 bg-transparent px-[18px] py-2 text-left"
              >
                <span className="min-w-0 flex-1 truncate text-[12.5px] leading-none text-ink2">
                  {edit.path}
                </span>
                {/* Same vocabulary as StatusDot: neon is the good outcome,
                    alert is the one that costs you something. */}
                {edit.added > 0 && (
                  <span className="flex-none text-[11.5px] leading-none text-neon-ink">
                    +{edit.added}
                  </span>
                )}
                {edit.removed > 0 && (
                  <span className="flex-none text-[11.5px] leading-none text-alert">
                    −{edit.removed}
                  </span>
                )}
              </button>
              {open && (
                <div className="px-[18px] pb-3">
                  <CodeBlock code={edit.diff} lang="diff" />
                </div>
              )}
            </div>
          );
        })}
        {/* Saying "and some more, unmeasured" keeps the counts above honest.
            Silently dropping them would make a partial list look complete. */}
        {uncounted > 0 && (
          <div className="px-[18px] py-2 text-[11.5px] leading-none text-ink3">
            {uncounted} more {uncounted === 1 ? "edit was" : "edits were"} too large to summarise
          </div>
        )}
      </div>
    </div>
  );
}
