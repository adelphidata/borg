---
name: skill-creator
description: "Create, refine, and keep Borg user skills lean when the user needs a reusable custom workflow."
category: core
triggers:
  - "create a skill"
  - "make a custom skill"
  - "teach borg this workflow"
  - "add an ad-hoc skill"
---

# Skill Creator

Create a user skill only when the workflow is reusable, non-trivial, and not already covered by Borg's built-in tools or existing skills.

## Decision Rule

- Do create a skill for repeated external CLI/API workflows, domain-specific operating procedures, or multi-step routines the user is likely to ask for again.
- Do not create a skill for one-off requests, basic shell commands, or workflows already covered by an existing skill.

## Authoring Rules

- Write the skill to `~/.borg/skills/<skill-name>/SKILL.md` via `apply_patch` with `target: "skills"`.
- Use a short hyphenated name.
- Keep the body concise and procedural.
- Put large docs under `references/` only when they are actually needed.
- Add `requires.bins`, `requires.env`, and `install` only when they materially help the workflow.
- Add `triggers` with short request phrases the user would realistically type.

## Minimal Template

````markdown
---
name: my-skill
description: "What it does and when to use it."
triggers:
  - "user phrase one"
  - "user phrase two"
requires:
  bins: ["curl"]
---

# My Skill

Explain the workflow Borg should follow.

## Examples

```bash
curl https://example.com
```
````

## Quality Bar

- Prefer a few sharp examples over long explanation.
- Assume Borg is already competent; only include workflow-specific guidance.
- If a script or reference file would prevent repeated verbose instructions, add it.
