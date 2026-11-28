# Undo points and `/rollback`

Borg runs tools without asking first. The control that makes that safe for
your files is the undo point: before a turn changes anything in the folder it
works in, Borg records the folder as it is. `/rollback` puts it back.

## Using it

| Command | What it does |
|---|---|
| `/rollback` | Put the files back to before the last turn that changed them. |
| `/rollback list` | Show the undo points of this conversation, newest first. |
| `/rollback <number>` | Go back to that point in the list. |

A rollback is itself an undo point: `/rollback` again brings the changes
back. The web and desktop apps also offer it as **Undo file changes** in the
`/` menu and the command palette.

`/rollback` moves files only. The conversation stays as it is; `/undo` is the
command that removes the last turn from the conversation, and it leaves the
files alone. After `/undo`, Borg tells you when the turn's file changes are
still on disk. It speaks only for the turn that was undone: changes an
earlier turn made are not mentioned, and neither are files you have already
moved with `/rollback`.

A rollback is refused while a turn is running or while sub-agents are still
at work. Stop them first (`/stop`).

A conversation that moved to another folder keeps the undo points of the
first one, and the list shows which folder each belongs to. `/rollback` with
no number only acts in the folder the conversation works in now; a point for
another folder is reached by its number.

## What an undo point holds

- Every file in the folder that is not ignored (`.gitignore` is honoured) and
  is not on the short built-in list (`node_modules/`, `.venv/`,
  `__pycache__/`, `.DS_Store`).
- Content byte for byte, and the executable bit. Other permission bits are
  not kept.
- A plain folder works as well as a repository.

What it does **not** hold, and therefore never touches on a rollback:

- Ignored files.
- Files over 10 MiB.
- Nested repositories (a folder with its own `.git`).
- Anything outside the folder. A tool that writes elsewhere is not covered.

When a rollback meets a file it does not track sitting where a file should
go back, it leaves that file and says so.

The ignore rules that count are the undo point's own. A turn that rewrites or
deletes `.gitignore` does not put the files it used to hide within reach: the
rollback puts `.gitignore` back first and leaves every file it ignores.

A file that cannot be written back or removed (a read-only folder, for
example) is named in the report. The other files are still put back. When
the file that cannot be put back is a `.gitignore`, the rollback removes no
file at all: without the undo point's rules it cannot tell a new file from
one they hid.

Two limits follow from "ignored files are not held":

- A file a turn makes and hides behind a new ignore rule in the same turn
  stays after a rollback, like any ignored file.
- An ignored file that a turn renames to a name that is not ignored is
  removed under its new name, and does not come back under the old one. The
  rollback's own undo point still has it: `/rollback` again.

One undo point is taken per turn, just before the first tool call that can
change files. A turn that only reads takes none. Sub-agents share their
parent's undo point. Each step of a `/goal` loop gets its own.

Undo points are only taken in a project folder. A session that runs in your
home folder, or in `/`, has none: recording those would mean hashing the
whole disk.

## Which folder a conversation works in

- **`borg` and `borg ask` in a terminal** work in the folder you ran them
  from.
- **`borg --worktree <name>` and editors (ACP)** work in the folder they
  name, and are confined to it.
- **The web and desktop apps, channels, scheduled tasks and the heartbeat**
  name no folder. Set one under **Settings → Conversation → Folder to work
  in** (`conversation.default_folder`, made if it is missing) and they work
  there. Project instructions and project skills are then read from that
  folder too. Without the setting they run in the folder the daemon was
  started in.
- A conversation you return to in the web or desktop app carries on in the
  folder it last worked in.
- A conversation that is still open keeps its folder. When you resume it from
  a terminal in another folder, Borg tells you where it works.

Your home folder, a folder above it, and `/` get no undo points: they are
places projects live in, not projects. Choose a project folder.

A turn waits at most 30 seconds for its undo point. When a folder is too
large for that, Borg says so once and the conversation makes no more undo
points there. Stopping a turn (`/stop`) also stops the wait.

## Where it lives

A Borg-owned store at `~/.borg/checkpoints/store`. It is never your project's
`.git`: no objects, refs or index entries are written there, and your
repository's hooks, filters and config are not run or read.

The store is control-plane data. The agent's tools can neither read nor
write it, the sandbox denies reading it, and `borg backup` leaves it out.

## How long undo points are kept

The nightly maintenance sweep drops undo points that were last used more
than 7 days ago, keeps at most 20 per folder, and keeps fewer per folder when
the store is over 500 MB. Disk space comes back at the following sweep.

## Turning it off

Settings → Advanced → Security → **Keep undo points for file changes**
(`checkpoints.enabled`, on by default). With it off no new undo points are
taken; the ones that exist can still be rolled back to until they age out.
