# Worktrees

A worktree is a second working directory for the same repository, on its own
branch, with its own index. Coding in one means an agent run never touches the
checkout you have open in your editor: no stashing, no surprise branch switch,
no half-applied patch on `main`.

## Quick start

```sh
borg worktree add fix-login     # create an isolated checkout on branch 'fix-login'
borg --worktree fix-login       # start a session that works in it
borg worktree list              # see every checkout of this repo
borg worktree remove fix-login  # delete the checkout (the branch is kept)
```

`borg wt` is an alias for `borg worktree`.

## Where they live

Managed worktrees land under `<$BORG_HOME>-worktrees/<repo>/<name>` (default
`~/.borg-worktrees/`). The name is slugified into a single safe path component,
so `borg worktree add "Fix Login Bug"` creates `fix-login-bug` on a branch of
the same name.

They are a *sibling* of the data directory, never inside it. Two things break if
they move under `~/.borg`: the sandbox denies writes to the whole data dir, so
the agent could read the checkout but never edit it; and `borg backup` tars up
`$BORG_HOME`, which would put every source checkout in every backup.

Use `--path <dir>` to put one somewhere else. Worktrees outside the managed root
— including any you created with `git worktree add` yourself — still show up in
`borg worktree list`, tagged `external`.

## Git is the source of truth

Borg keeps no database table of worktrees. `git worktree list` already records
every checkout and cannot drift from reality, so a worktree created by hand is
visible to Borg, and one removed by hand does not leave a phantom row behind.

"Managed" is a path test, not a stored flag: a checkout is managed if it lives
under the worktrees root.

## The `--worktree` flag

`--worktree <name>` (short `-w`) is global, so it works with the TUI and with
`borg ask`:

```sh
borg -w fix-login                       # interactive session in the worktree
borg ask "run the tests" -w fix-login   # one-shot in the worktree
```

It is find-or-create: the first use creates the worktree, later uses attach to
it. The name resolves against the directory name, the branch, or a full path, so
the same word keeps landing in the same tree.

The flag does two things, and both are needed. It moves the `borg` process into
the worktree, which is what scopes session resume and workspace identity; and it
sends that directory to `borgd` as the session's workspace, which is what puts
the *agent* there. `borgd` is long-lived and has its own current directory, so
without the second half the agent would edit the main checkout while the client
sat in the worktree.

`borg worktree ...` itself ignores `--worktree` — resolving a worktree in order
to manage worktrees would create the very thing you may be about to remove.

`borg acp` **refuses** it. ACP sessions take their directory from the editor, per
session, so the flag could not be honored; refusing beats ignoring, which would
leave the editor patching the main checkout. Point the editor at the worktree
path instead.

A name that matches only the main checkout is never attached to. `borg -w main`
does not silently run in your live tree — it falls through to creation, which
git refuses because a branch cannot be checked out twice.

## Inside the TUI

```
/worktree                    where this session is working, and what else exists
/worktree new <name>         create an isolated checkout on its own branch
/worktree remove <name>      delete one (add --force to discard changes)
```

`/wt` is the alias. The overview marks the checkout the session is actually in,
which is the only reliable way to tell an isolated session from one editing the
main tree.

A running session does not move between worktrees: its workspace is bound when
the session opens. `/worktree new` therefore prints the `borg --worktree <name>`
command that starts a session in the new tree.

## Safety

Removal refuses four cases:

| Case | Behavior |
|---|---|
| The main checkout | Refused always. `--force` does not override it. |
| A locked worktree | Refused. Run `git worktree unlock <path>` first. |
| Uncommitted changes | Refused unless `--force`, which discards them. |
| An `external` checkout | A bare name or branch never reaches it. Give its full path. |

That last row is why removal resolves names only among worktrees Borg manages: a
checkout you made yourself is yours, and deleting it because a word happened to
match would be a surprise. The full path still works — typing it is a deliberate
act rather than a guess.

Untracked files count as uncommitted work — they are just as unrecoverable once
the directory is gone. Removing a worktree never deletes its branch.

## Pruning

`borg worktree prune` forgets administrative entries whose directory is already
gone (for example after `rm -rf`). It never deletes a directory that still
exists, and it reports exactly which entries it dropped.

Borg passes `--expire now` rather than git's three-month default: a `prune` that
reported nothing and changed nothing would read as broken.

## Commands

| Command | Purpose |
|---|---|
| `borg worktree` / `list` | Every checkout of this repository, tagged `main` / `locked` / `stale` / `external` |
| `borg worktree add <name>` | Create a checkout on a new branch |
| `borg worktree add <name> -b <branch>` | Name the branch explicitly |
| `borg worktree add <name> --from <commit>` | Fork the branch from a specific commit, tag, or branch |
| `borg worktree add <name> --existing` | Check out a branch that already exists (refuses `--from`) |
| `borg worktree add <name> --path <dir>` | Put the checkout somewhere other than the managed root |
| `borg worktree add <name> --print-path` | Print only the path, for `cd "$(...)"` |
| `borg worktree remove <target> [-f]` | Delete a checkout by name, branch, or path |
| `borg worktree prune` | Forget entries whose directory is gone |
