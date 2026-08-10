# packages/ui

Local rules only. Workspace layout, the transport seam, build order, and `just` targets are in the root `CLAUDE.md`.

**Read "UX — Progressive Disclosure" in the root `CLAUDE.md` before you add a component here.** It is the design thesis for every surface this package paints.

## Invariants

**Presentational only.** A component here takes its data through props and returns markup. No `useQuery`, no `createBorgClient`, no store imports — the moment a design-system component fetches, it can only be reused where that fetch makes sense, which is nowhere else. Derivation lives in `packages/app`; this package renders the answer.

**One vocabulary per signal.** `StatusDot` owns what a state looks like, and callers pass a tone rather than a colour class. A page that reaches for `bg-amber-400` has invented a sixth meaning nobody else knows, and the vocabulary stops meaning anything. Adding a state means adding a tone here, with a comment saying what it is for.

**Motion has to earn its place.** A dot that pulses pulls the eye, which is right for the one thing a person is waiting on and wrong for a column of list rows. Prefer a difference the eye can read at rest — filled against hollow, present against absent.

## Tooltips

A tooltip is worth its interruption only when hovering teaches something the screen does not already say. Three rules follow from that, and all three have been violated here before:

- **Never on a dismiss control.** An × closes things. A tip that says "Close" is the label read back to the user, and it covers the thing they are trying to close.
- **Never paraphrasing a visible label.** If the button says "Archive", the tip must not say "Archive this" or "Click to archive". Repeating a label teaches nothing and trains people to ignore tips that would have.
- **Never on a menu trigger.** A kebab opens a menu whose items name themselves. The menu is the explanation.

What is left is the legitimate case: an icon-only control whose meaning is not obvious, or a status that has a reason worth reading. `Archive conversation` on the sidebar's hover × survives on the first ground — the glyph alone reads as "delete", and the difference between archiving and deleting is exactly what a person needs to know before they click.

Prefer an `aria-label` alone when the control is obvious to a sighted user but opaque to a screen reader. A label is not a tooltip and costs nothing.
