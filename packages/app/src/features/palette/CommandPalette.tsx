import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { SearchIcon } from "@borg/ui";
import { dispatchKeybind, hintFor, useKeybind } from "../../lib/keybinds";
import { useQuery } from "../../lib/useDaemon";
import { useSettings } from "../../lib/settings";
import { routes } from "../../routes";
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

  const commands = useMemo<Command[]>(() => {
    const go: Command[] = routes.map((route) => ({
      id: `route:${route.id}`,
      label: `Go to ${route.label}`,
      hint: "Navigate",
      run: () => navigate(route.path),
    }));

    const actions: Command[] = [
      {
        id: "action:new",
        label: "New conversation",
        // Both the hint and the behaviour come from the keybind layer, so the
        // palette can never advertise or perform a stale version of either.
        hint: hintFor("new-conversation"),
        keywords: "chat start fresh",
        run: () => dispatchKeybind("new-conversation"),
      },
    ];

    const recents: Command[] = (sessions.state === "ready" ? sessions.data : []).map((s) => ({
      id: `session:${s.id}`,
      label: s.title || "Untitled",
      hint: "Conversation",
      run: () => navigate(`/c/${s.id}`),
    }));

    const registry = settings.settings.state === "ready" ? [...settings.settings.data.keys()] : [];
    const settingRows: Command[] = registry.map((key) => ({
      id: `setting:${key}`,
      label: key,
      hint: "Setting",
      // Dots and underscores hide word boundaries from the subsequence match.
      keywords: key.replace(/[._]/g, " "),
      run: () => navigate(`/settings/key/${encodeURIComponent(key)}`),
    }));

    return [...actions, ...go, ...recents, ...settingRows];
  }, [navigate, sessions.state, sessions.data, settings.settings]);

  const results = useMemo(() => rank(commands, query).slice(0, 40), [commands, query]);

  // A stale cursor from a longer list would point past the end of a shorter one.
  useEffect(() => setCursor(0), [query]);

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
    setOpen(false);
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
              <span className="min-w-0 flex-1 truncate text-[13.5px] leading-[1.35] text-ink">
                {command.label}
              </span>
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
