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

Create a user skill only for reusable, non-trivial workflows not already
covered by built-ins. Skip for one-off requests or basic shell commands.

Write to `~/.borg/skills/<skill-name>/SKILL.md` via `apply_patch` with
`target: "skills"`. Short hyphenated name; concise procedural body; large
docs under `references/` only if needed. Add `requires.bins`/`requires.env`/
`install` only when material. `triggers` are short request phrases the user
would realistically type.

Prefer a few sharp examples over prose. Assume Borg is competent — include
only workflow-specific guidance.

## Template

````markdown
---
name: my-skill
description: "What it does and when to use it."
triggers:
  - "user phrase one"
requires:
  bins: ["curl"]
---

# My Skill

Workflow in one or two sentences.

```bash
curl https://example.com
```
````
