---
name: subagent-driven-development
description: "Implement a plan via parallel sub-agents with a two-stage review per task (spec compliance + code quality)."
triggers:
  - "implement the plan with subagents"
  - "delegate this work"
  - "fan out to subagents"
  - "two-stage review"
  - "subagent-driven development"
requires:
  bins: []
---

# Subagent-Driven Development

A workflow for executing a multi-task plan with `spawn_agent` so the parent
stays a coordinator and each child does focused, reviewable work.

## When to use

- The plan has **3 or more independent tasks** that touch different
  surfaces. (Below that, just do them inline — delegation overhead isn't
  worth it.)
- You want **review pressure on every change** — each task gets a spec
  check and a quality check before it's accepted.
- The parent's context is already long and you want to keep
  implementation noise out of it.

If the work is one tightly-coupled refactor across a single subsystem,
don't delegate — a single agent with the whole picture will out-perform a
fan-out.

## The loop

For each task in the plan:

1. **Implementer** — `spawn_agent` with `blocking: true`, `role: "coder"`.
   Pass the task as `message`, and put plan excerpts / file paths / prior
   findings in `context`. Narrow tools with `toolsets` if the task is
   bounded (e.g. `["read_file", "apply_patch", "run_shell"]`).
2. **Spec-compliance reviewer** — `spawn_agent` `blocking: true`,
   `role: "researcher"`. Goal: *"Does the implementation satisfy
   `<verbatim spec excerpt>`? Return PASS or a numbered list of misses."*
   Pass the implementer's `result` and `files_written` in `context`.
3. **Code-quality reviewer** — same shape. Goal: *"Review for clarity,
   safety, hidden coupling, and the project's test-quality rules. Return
   PASS or a numbered list of findings."*
4. **Fix loop** — if either review fails, spawn one more implementer with
   the findings as `context`. Repeat (2) and (3). Cap at 2 fix rounds; if
   still failing, escalate to the parent.

Once every task passes both reviews:

5. **Final integration review** — one more `spawn_agent` reading the
   merged diff. Catches cross-task interactions the per-task reviewers
   missed.

## Fan-out

When tasks are independent (different files, no shared state), batch
them with `spawn_agent` `tasks: [...]`. The parent waits for all and
gets a `results` array with `files_written` per child — re-read any of
those paths before continuing.

## Anti-patterns

- **Skipping reviews because the diff looks fine.** The reviewers exist
  to catch the things you'd miss after staring at the same plan all day.
- **Two implementers on the same file in one batch.** Their patches
  will fight; serialize them with `blocking: true` instead.
- **Asking a child to read the plan.** Paste the relevant slice into
  `context`. The child has no parent history.
- **`role_kind: orchestrator` by default.** Leaf is right almost always
  — grandchildren burn context budget and the parent loses visibility.
  Only use orchestrator when the child genuinely needs to fan out further.
- **Continuing past an open finding.** Reviewers exist to gate, not
  decorate. A red review blocks the next task.

## Tool cheatsheet

```text
# Single task, blocking, narrow tools, with context
spawn_agent {
  message: "Add a `--dry-run` flag to `borg backup`",
  context: "Source: crates/interface/src/cli/backup.rs. The dry-run flag should print the plan and exit 0 without writing.",
  role: "coder",
  toolsets: ["read_file", "apply_patch", "run_shell"],
  blocking: true
}

# Batch of independent tasks
spawn_agent {
  tasks: [
    { goal: "Implement task A", role: "coder", context: "..." },
    { goal: "Implement task B", role: "coder", context: "..." },
    { goal: "Implement task C", role: "coder", context: "..." }
  ]
}

# Reviewer (read-only)
spawn_agent {
  message: "Review the diff against the spec. PASS or list misses.",
  context: "<spec excerpt>\n\n<implementer's result>\n\nFiles written: ...",
  role: "researcher",
  toolsets: ["read_file", "run_shell"],
  blocking: true
}
```
