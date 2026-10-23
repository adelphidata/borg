---
name: apple-notes
description: "Read, search, create, and append to Apple Notes on macOS (synced to iPhone and iPad through iCloud), using the memo CLI and AppleScript. Use when the user wants a note in Notes.app. For Markdown note files use the notes or obsidian skills instead."
category: utilities
triggers:
  - "add this to my apple notes"
  - "create a note in notes"
  - "search my notes app"
  - "read my apple note"
  - "append to my note"
requires:
  bins: ["memo"]
os: [macos]
install:
  memo:
    brew: "antoniorodr/memo/memo"
    url: "https://github.com/antoniorodr/memo"
---

# Apple Notes

`memo` reads Notes.app and prints notes as Markdown. Its add, edit, delete,
and search commands open an editor or a picker, so write through
`osascript` instead, and pass text as arguments (never splice it into the
script). The first run asks the user to allow access to Notes (System
Settings, Privacy & Security, Automation).

Prefer Apple Notes when the user wants the note on their phone. For Borg's own
memory use `write_memory`; for Markdown files use the `notes` skill.

## Read

```bash
memo notes -fl              # folder tree
memo notes                  # every note, numbered: "12. Folder - Title"
memo notes -f "Recipes"     # one folder
memo notes -v 12            # show note 12 as Markdown
memo notes -nc              # skip memo's cache (after changes made elsewhere)
```

Numbers change when notes are added or removed: list again before `-v`.

## Search (title and text)

```bash
osascript - "wifi" <<'EOF'
on run argv
  tell application "Notes"
    set out to ""
    repeat with n in (notes whose body contains (item 1 of argv))
      set out to out & (name of n) & "  [" & (name of container of n) & "]" & linefeed
    end repeat
    return out
  end tell
end run
EOF
```

## Create

The body is HTML; the first line becomes the title. Escape `&`, `<`, and `>`
in user text.

```bash
osascript - "Notes" "<h1>Groceries</h1><div>Milk</div><div>Eggs</div>" <<'EOF'
on run argv
  tell application "Notes" to make new note at folder (item 1 of argv) with properties {body:(item 2 of argv)}
end run
EOF
```

Use `<ul><li>...</li></ul>` for lists and `<b>` for bold. To target an
account: `folder "Notes" of account "iCloud"`. New folder:
`tell application "Notes" to make new folder with properties {name:"Trips"}`.

## Append

```bash
osascript - "Groceries" "<div>Butter</div>" <<'EOF'
on run argv
  tell application "Notes"
    set n to first note whose name is (item 1 of argv)
    set body of n to (body of n) & (item 2 of argv)
  end tell
end run
EOF
```

Rewriting the body of a note that holds images, drawings, or attachments can
drop them: check with `memo notes -v N` first, and for such notes create a new
note instead. Delete a note only when the user asks, and confirm the exact
title first (`delete (first note whose name is "...")`); it goes to Recently
Deleted.

## Limits

- Locked (password-protected) notes cannot be read or changed.
- Shared notes and checklists come through as plain HTML lists.
- Name matches are exact and case-sensitive in `whose name is`; search first
  when unsure.
