---
name: code-simplify
description: "Review code for safe simplifications, AI-slop, and silently-swallowed errors; apply by risk tier."
triggers:
  - "simplify this code"
  - "clean up this file"
  - "remove dead code"
  - "is this swallowing errors"
  - "review for slop"
  - "tidy this up"
requires:
  bins: ["git"]
---

# Code Simplify

Review changed code (or a named file/scope) for three classes of problem —
**slop**, **silent failures**, and **needless complexity** — then apply the
fixes by risk tier. The goal is fewer lines that say the same thing, with no
behaviour change and no swallowed error.

## When to use

- After a feature lands and the diff reads noisier than it needs to.
- When asked to "clean up", "tidy", "simplify", or "remove dead code".
- When you suspect an error is being silently discarded.

If the request is "find bugs", that's a correctness review, not this. This skill
preserves behaviour — it never changes what the code *does*, only how plainly it
says it.

## Scope first

Default scope is the **current diff** (`git diff` / `git diff --staged`), not the
whole repo. A whole-file or whole-crate sweep only when explicitly asked — an
unscoped rewrite buries the real findings and is hard to review.

```bash
git diff --staged            # what's about to land
git diff HEAD~1..HEAD        # the last commit
```

## What to look for

### 1. Silently-swallowed errors (highest priority)

An error that's discarded without a log or a propagation is a latent incident.
Flag every one — these are safety findings, never auto-deleted.

- Rust: `let _ = fallible()`, `.ok()` / `.unwrap_or_default()` on an op that
  matters, `Err(_) => {}` arms, `if let Ok(x) = ... {}` with no `else`.
- Other langs: empty `catch {}`, `except: pass`, `.catch(() => {})`,
  a returned error value that's never checked.

The bar (from this project's invariants): `let _ =` / `.ok()` are acceptable
**only** for truly best-effort ops — typing indicators, terminal cleanup in
`Drop`, temp-file removal. Everything else must propagate with `?` or
`tracing::warn!` on the error path. If success is reported to the user, it must
actually have succeeded.

### 2. AI-slop

Machine-written code that a human wouldn't leave behind:

- Comments that restate the code (`// increment i`), or stale phase/task/version
  markers left over from a prior edit.
- Defensive checks that can't fire — a null/`None` guard on a value the type
  system already guarantees, a re-validation of an already-validated input.
- Escape hatches that erase safety — `as any`, `unwrap()` where `?` fits,
  `#[allow(...)]` masking a real lint.
- The same thing done two ways in adjacent code (inconsistent patterns).

### 3. Needless complexity

- A local used once — inline it. A function called once that doesn't aid
  readability — consider folding it in.
- A nested match/if-else that collapses to a `match` arm, `?`, or early return.
- Hand-rolled iteration that an iterator adapter expresses directly.
- Dead code: unreachable branches, unused fields, a feature flag with one value.

## Chesterton's Fence

Before flagging anything for **removal**, find out why it's there.

```bash
git blame -L <start>,<end> path/to/file.rs
git log --oneline -S 'the_symbol' -- path/to/file.rs   # when was it introduced, and why
```

If `git blame` shows the line was added deliberately (a commit message naming a
bug, an edge case, a regression test), do **not** remove it — downgrade the
finding to a question for the human. Low confidence means escalate, never guess.
"I don't see why this is needed" is not the same as "this is not needed".

## Risk tiers — how to apply

Tag every finding, then act by tier:

| Tier | Meaning | Action |
|------|---------|--------|
| **SAFE** | Behaviour provably unchanged; no public API touched (slop comment, dead local, redundant guard the type system covers). | Apply directly. |
| **CAREFUL** | Likely safe but needs a check (collapsing control flow, inlining, swapping a loop for an adapter). | Apply, then build + run the nearest tests for that file before moving on. |
| **RISKY** | Changes a signature, touches error-handling semantics, or `git blame` is ambiguous. | Do **not** apply. Report it and let the human decide. |

A swallowed-error fix is **never** SAFE — surfacing an error that was hidden is a
behaviour change. Treat it CAREFUL (add the `?`/`warn!`, then test) or RISKY
(if the right handling isn't obvious) and say so.

## Output

Report findings as a list, each line:

```
path/to/file.rs:42  →  <problem>  →  <fix>   | confidence: H/M/L | risk: SAFE/CAREFUL/RISKY
```

Then apply the SAFE and CAREFUL tiers, build, run the affected tests, and list
the RISKY findings separately for the human. State plainly what you changed and
what you left for them — don't report a clean sweep if RISKY items remain.

## Anti-patterns

- **Mixing simplification with behaviour changes.** If you found a bug, say so
  separately — don't smuggle a fix into a "cleanup".
- **Deleting on a hunch.** No `git blame`, no removal.
- **Silencing the slop instead of fixing it.** An `#[allow(dead_code)]` is not a
  way to resolve dead code.
- **Auto-applying a swallowed-error "fix" you're unsure of.** Surfacing an error
  badly (panicking where it should warn) is worse than the original. RISKY →
  escalate.
- **Unscoped rewrites.** Stay in the diff unless told otherwise.
