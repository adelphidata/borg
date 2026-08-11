import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { CheckIcon, SearchIcon, useTheme } from "@borg/ui";
import { dispatchKeybind, hintFor, useKeybind } from "../../lib/keybinds";
import { useQuery } from "../../lib/useDaemon";
import { useSettings } from "../../lib/settings";
import { routes } from "../../routes";
import { buildCommands } from "./commands";
import { rank, type Command } from "./score";

/**
 * ⌘K.
 *
 * The reason the sidebar can stay at four entries: every page, every recent
 * conversation, and every setting is one chord and a few letters away, so new
 * surfaces do not have to earn permanent navigation to be reachable.
 *
 * Opens with the daemon data it already has — the settings registry is loaded
 * once by `SettingsProvider` — so there is no spinner between the chord and a
 * usable list.
 */
export function CommandPalette() {
  const navigate = useNavigate();
  const settings = useSettings();
  const { preference, setPreference } = useTheme();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [cursor, setCursor] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  useKeybind("command-palette", () => setOpen((v) => !v), { inInputs: true });
  // While open, Escape belongs to the palette rather than to a running turn.
  useKeybind("cancel-turn", () => setOpen(false), { enabled: open, inInputs: true });

  // Conversations are only fetched while the palette is actually open.
  const sessions = useQuery(
    async (borg) => (open ? (await borg.admin.listSessions({})).sessions : []),
    [open],
  );

  // Values written but not yet confirmed by a fresh snapshot. Without this a
  // toggle reads its old value until the write round-trips, so pressing Enter
  // twice writes the same value twice and the second press silently does
  // nothing. Cleared per key when the snapshot catches up, or on failure.
  const [pending, setPending] = useState<Map<string, string>>(new Map());
  const [saveError, setSaveError] = useState<string>();

  const forget = (key: string) =>
    setPending((prev) => {
      const next = new Map(prev);
      next.delete(key);
      return next;
    });

  const saveSetting = useCallback(
    (key: string, value: string) => {
      setSaveError(undefined);
      setPending((prev) => new Map(prev).set(key, value));
      void settings.save(key, value).then((ok) => {
        // Reporting success we did not have would leave the row showing a
        // state the daemon never took.
        if (!ok)
          setSaveError(`Could not change that setting. ${settings.saveError?.message ?? ""}`);
        forget(key);
      });
    },
    [settings.save, settings.saveError],
  );

  const snapshot = settings.settings.state === "ready" ? settings.settings.data : undefined;
  const effective = useMemo(() => {
    if (!snapshot || pending.size === 0) return snapshot;
    const merged = new Map(snapshot);
    for (const [key, value] of pending) {
      const current = merged.get(key);
      if (current) merged.set(key, { ...current, value });
    }
    return merged;
  }, [snapshot, pending]);

  // Only whether a query exists matters, not what it is. Depending on the text
  // would rebuild every one of ~500 rows on each keystroke.
  const searching = query.trim() !== "";
  const commands = useMemo<Command[]>(
    () =>
      buildCommands(
        {
          navigate,
          dispatch: dispatchKeybind,
          hintFor,
          settings: effective,
          saveSetting,
          sessions: sessions.state === "ready" ? sessions.data : [],
          themePreference: preference,
          setThemePreference: setPreference,
          searching,
        },
        routes,
      ),
    [
      navigate,
      sessions.state,
      sessions.data,
      effective,
      saveSetting,
      preference,
      setPreference,
      searching,
    ],
  );

  const results = useMemo(() => rank(commands, query).slice(0, 40), [commands, query]);

  // A stale cursor from a longer list would point past the end of a shorter one.
  useEffect(() => setCursor(0), [query]);

  useEffect(() => {
    if (!open) setSaveError(undefined);
  }, [open]);

  useEffect(() => {
    if (!open) setQuery("");
  }, [open]);

  // Keep the highlighted row in view when arrowing past the fold.
  useEffect(() => {
    listRef.current?.children[cursor]?.scrollIntoView({ block: "nearest" });
  }, [cursor]);

  if (!open) return null;

  const choose = (command: Command | undefined) => {
    if (!command) return;
    // A toggle or a theme is picked by watching it land, so those rows keep the
    // palette open. Closing would hide the only confirmation the row gives.
    if (!command.keepOpen) setOpen(false);
    command.run();
  };

  return (
    <div
      className="fixed inset-0 z-50 flex justify-center bg-black/40 pt-[14vh]"
      onClick={() => setOpen(false)}
    >
      <div
        role="dialog"
        aria-label="Command palette"
        onClick={(e) => e.stopPropagation()}
        className="flex max-h-[62vh] w-[min(560px,92vw)] flex-col overflow-hidden rounded-[14px] border border-line bg-surface2 shadow-(--frameShadow)"
      >
        <div className="flex flex-none items-center gap-2.5 border-b border-line2 px-4 py-3">
          <SearchIcon size={15} />
          <input
            autoFocus
            className="min-w-0 flex-1 border-0 bg-transparent text-[14px] text-ink outline-none select-text placeholder:text-ink3"
            placeholder="Search commands, conversations and settings"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setCursor((c) => Math.min(results.length - 1, c + 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setCursor((c) => Math.max(0, c - 1));
              } else if (e.key === "Enter") {
                e.preventDefault();
                choose(results[cursor]);
              }
            }}
          />
        </div>

        {/* A row that keeps the palette open so you can watch a toggle land
            has to say when it did not. Otherwise a failed write looks exactly
            like a slow one: the state simply never changes. */}
        {saveError && (
          <div
            role="alert"
            className="flex-none border-b border-line2 px-4 py-2 text-[11.5px] leading-[1.4] text-alert"
          >
            {saveError.trim()} Check that the Borg app is running, then try again.
          </div>
        )}

        <div ref={listRef} className="min-h-0 flex-1 overflow-y-auto py-1.5">
          {results.length === 0 && (
            <div className="px-4 py-6 text-center text-[12.5px] leading-[1.5] text-ink3">
              Nothing matches.
            </div>
          )}
          {results.map((command, i) => (
            <button
              key={command.id}
              type="button"
              onMouseEnter={() => setCursor(i)}
              onClick={() => choose(command)}
              className={`flex w-full cursor-pointer items-center gap-3 border-0 px-4 py-2 text-left ${
                i === cursor ? "bg-surface3" : "bg-transparent"
              }`}
            >
              <span className="min-w-0 truncate text-[13.5px] leading-[1.35] text-ink">
                {command.label}
              </span>
              {command.detail && (
                <span className="flex-none text-[11.5px] leading-none text-ink3">
                  {command.detail}
                </span>
              )}
              <span className="min-w-0 flex-1" />
              {command.active && <CheckIcon size={14} role="img" aria-label="Current" />}
              {command.hint && (
                <span className="flex-none text-[11px] leading-none text-ink3">{command.hint}</span>
              )}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}
