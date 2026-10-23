---
name: debugging
description: "Find the root cause of a bug, failing test, crash, flaky behaviour, or build break before changing code: build a command that reproduces it, narrow it down, test ranked hypotheses one at a time, then fix with a regression test. Use when a fix is not obvious, or a first fix did not work."
category: developer
triggers:
  - "debug this"
  - "why is this failing"
  - "find the root cause"
  - "this test is flaky"
  - "the fix didn't work"
---

# Debugging

No fix before a root cause. A guess that makes the symptom go away usually
moves the bug somewhere else.

## 1. Build a loop that goes red

Before you read code to form theories, get one command that fails on the
user's exact symptom and will pass once it is fixed. It must be fast enough to
run often, and specific: it asserts the symptom, not merely "no crash". Try,
in about this order:

1. A failing test at the seam that reaches the bug.
2. A `curl` against the running service, or a CLI call with fixed input whose
   output you diff against the expected output.
3. A replay of the captured input: request body, event, log line, file.
4. A throwaway script that starts the smallest slice of the system and calls
   the failing path.
5. `git bisect run <loop>` when it worked at some earlier commit.
6. A differential run: old version against new, or config A against config B.

Then tighten it: cache setup, pin time and random seeds, isolate files and
network. For a flaky bug the goal is a high reproduction rate, not perfection:

```bash
for i in $(seq 1 100); do <test command> >/dev/null 2>&1 || { echo "failed on run $i"; break; }; done
```

If you cannot reproduce it, gather more evidence. Do not guess.

## 2. Read the evidence

- Read the whole error and stack trace: file, line, error code. Search for the
  message: `rg -n "exact error text"`.
- What changed? `git log --oneline -15`, `git diff`, and
  `git log -p --follow -- <file>`, plus dependency bumps and config edits.
- Across components (client, API, worker, database, CI), log what enters and
  leaves each boundary once, so you see *where* the value goes wrong before
  you ask *why*.
- Trace a bad value back to where it is made, and fix it there, not where it
  surfaces.

## 3. Shrink the case

With the loop red, remove inputs, steps, config, and data one at a time,
re-running after each cut. Stop when every remaining piece is needed for the
failure. The minimal case often becomes the regression test.

Then find a similar path that works and list every difference, however small.

## 4. Test ranked hypotheses

- Write 3 to 5 hypotheses. Rank them by likelihood and by how cheap they are
  to rule out. Each must predict something: "if X, then changing Y makes Z".
- If the user is around, show the list first; they may know which is wrong.
- Test one at a time with the smallest probe. Change one thing per run.
- Prefer a debugger or REPL to scattered prints. Tag every temporary log line
  (`[dbg-7f3a]`) so one search removes them all.
- A failed hypothesis gets crossed out, not patched over. Never stack a second
  change on an unproven first one.
- If you do not understand something, say so, research it (`web_search`,
  the library's source or docs), or ask.

## 5. Fix it

1. Turn the loop into a regression test that fails now.
2. Make one change that addresses the root cause. No refactors or "while I'm
   here" edits in the same change.
3. Run the regression test, then the wider suite.
4. Remove the tagged debug lines.

## The rule of three

After three failed fixes, stop. When each fix exposes a new problem somewhere
else, the design is the problem: shared state, hidden coupling, or a wrong
assumption. Explain this to the user and agree on a direction before a fourth
attempt.

## Warning signs: go back to step 1

"Quick fix now, investigate later." "Let me just try X." Several changes in one
run. Skipping the test because you checked by hand. "It's probably X" with no
evidence. A fix proposed before the data flow was traced.

## Parallel investigation

For a bug that crosses several components, `spawn_agent` can look at each one
at once. Give each child the exact error, the loop command, and the files,
and ask it to report findings without changing code.

## Report

Root cause in one or two sentences, the evidence that proves it, the fix, the
regression test, and anything you ruled out along the way.
