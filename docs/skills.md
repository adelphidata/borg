# Skills

Skills are instruction bundles that teach the agent how to use external CLI tools. They are distinct from "tools" -- skills are text-based instructions injected into the system prompt, while tools are sandboxed executable scripts.

## How skills work

1. At each turn, available skills are loaded and formatted into the system prompt
2. The agent reads the skill instructions and can use `run_shell` to execute the described CLI commands
3. Skills declare their requirements (binaries, env vars) which are checked at load time
4. Unavailable skills (missing requirements) are still listed but flagged as unavailable
5. Credential injection: resolved credentials from `[credentials]` config are injected as env vars into `run_shell` commands

## Built-in skills

Forty-five skills are embedded in the binary. Only a core set is enabled by default; turn another on with `borg skills enable <name>`:

| Skill | Requirements | Description |
|-------|-------------|-------------|
| `skill-creator` | -- | Create lean reusable Borg user skills when needed |
| `slack` | `curl`, `SLACK_BOT_TOKEN` | Send messages to Slack channels |
| `discord` | `curl`, `DISCORD_BOT_TOKEN` | Send messages to Discord channels |
| `github` | `gh` | Interact with GitHub (issues, PRs, repos) |
| `weather` | `curl` | Get weather information |
| `git` | `git` | Git operations (commit, branch, diff, log) |
| `search` | `curl` | Web search integration |
| `docker` | `docker` | Docker container management |
| `database` | varies | SQL/database operations |
| `notes` | -- | Note-taking and organization |
| `calendar` | varies | Calendar operations |
| `1password` | `op` | 1Password secret management |
| `browser` | varies | Web browser automation instructions |
| `email` | varies | Email operations |
| `scheduler` | -- | Scheduled task management |
| `paperclip` | `npx`, `curl` | Orchestrate multi-agent autonomous companies via the Paperclip control plane |
| `linear` | `curl`, `jq`, `LINEAR_API_KEY` | Manage Linear issues, projects, and cycles via the GraphQL API |
| `pdf` | `pdftotext` | Read, extract, split, and merge PDF files |
| `reminders` | `reminders` (CLI) | Manage native macOS Reminders via reminders-cli |
| `borg-cli` | -- | Drive the borg CLI to self-configure and diagnose: settings, skills, memory, tasks, channels, status |
| `code-simplify` | `git` | Review code for safe simplifications, AI-slop, and silently-swallowed errors; apply by risk tier |
| `healthcheck` | -- | Audit host security posture — SSH config, firewall, listening ports, package updates, cron |
| `notion` | `curl`, `jq`, `NOTION_API_KEY` | Read and write Notion pages, databases, and blocks via the REST API |
| `obsidian` | -- | Work with Borg's markdown knowledge files as an Obsidian-friendly workspace |
| `stripe-link` | `link-cli` | Fetch one-time payment credentials from a Stripe Link wallet to complete purchases on the user's behalf |
| `subagent-driven-development` | -- | Implement a plan via parallel sub-agents with a two-stage review per task |
| `video-frames` | `ffmpeg`, `ffprobe` | Extract frames, thumbnails, contact sheets, GIFs, and short clips from videos |
| `inbox-triage` | -- (uses `gmail` or `himalaya`) | Triage an inbox into replies, actions, waiting, and noise; drafts replies for approval |
| `weekly-review` | -- | Weekly look back and capacity-aware plan across calendar, tasks, notes, and flagged mail |
| `action-items` | -- | Cited decisions, owners, and deadlines from meeting notes, transcripts, or documents |
| `price-watch` | -- | Scheduled price or stock watch on an exact item, alerting once when the condition is met |
| `humanizer` | -- | Rewrite text to drop machine-written tics and match the user's voice |
| `x-twitter` | `xurl` | Search, read, and post on X (Twitter) with the official xurl CLI |
| `word` | `python3` | Create and edit Word .docx files (python-docx in `~/.borg/venv`) |
| `excel` | `python3` | Create and edit Excel .xlsx workbooks with live formulas and charts (openpyxl) |
| `powerpoint` | `python3` | Create and edit PowerPoint .pptx decks (python-pptx) |
| `youtube` | `python3` | Summaries, chapters, and quotes from a YouTube video's captions |
| `rss` | `python3` | Read RSS/Atom/JSON feeds, discover a site's feed, scheduled digests |
| `apple-notes` | `memo` (macOS) | Read, search, create, and append to Apple Notes |
| `hue` | `openhue` | Control Philips Hue lights, rooms, and scenes on the local network |
| `find-my` | -- (macOS) | Locate Apple devices and AirTags by reading the Find My app |
| `maps` | `curl`, `jq` | Geocoding, nearby places, routes, and time zones from OpenStreetMap, no key |
| `stocks` | `curl`, `jq` | Stock, index, currency, and crypto quotes and history, no key |
| `reddit` | `python3` | Read subreddits, search, threads, and users through Reddit's public feeds |
| `debugging` | -- | Root-cause debugging: reproduce, narrow, test hypotheses, then fix with a regression test |

## User skills

User-created skills live at `~/.borg/skills/<name>/SKILL.md`. The agent creates these via `apply_patch` with `target: skills`.

User skills must use a unique name. Built-in name collisions are rejected by the loader.

## Project skills

A repository can ship its own skills, so instructions for working on that repo
travel with it instead of every teammate installing them by hand. They are read
from the session's workspace:

- `<workspace>/.borg/skills/<name>/SKILL.md`
- `<workspace>/.agents/skills/<name>/SKILL.md` — the cross-tool convention, so a
  repo that already ships skills for another agent harness works unchanged.

Precedence is **built-in > project > user**. A project skill shadows a personal
skill of the same name, because it describes how to work on *this* checkout. It
can never shadow a built-in.

Project skills go through the same security scan as user skills, and there is no
prompt to approve them — the scan is the boundary. A skill whose content scans as
high-risk (prompt injection, credential exfiltration, invisible-unicode
smuggling) is excluded from the catalog, from `skill_list`, and from invocation.
A flagged one loads wrapped in an injection warning. This matters more here than
for user skills: a repo arrives with a `git pull`, not by your own hand, and its
skills change every time you pull.

Only a session that has a workspace loads them. `borgd`'s own working directory
is not any session's workspace, so no workspace means no project skills rather
than a guess at which repo was meant.

Turn the tier off entirely with:

```sh
borg settings set skills.project_discovery false
```

## SKILL.md format

```markdown
---
name: my-skill
description: "What it does and when to use it."
triggers:
  - "user phrase one"
  - "user phrase two"
requires:
  bins: ["curl", "jq"]
  env: ["API_TOKEN"]
  any_bins: ["chrome", "chromium"]
os: ["macos", "linux"]
install:
  jq:
    brew: "jq"
    apt: "jq"
---

# My Skill

Instructions for the agent on how to use this skill.

## Examples

\`\`\`bash
curl -H "Authorization: Bearer $API_TOKEN" https://api.example.com/data | jq '.results'
\`\`\`
```

### Frontmatter fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Skill identifier (should match directory name) |
| `description` | yes | When and why the agent should use this skill |
| `triggers` | no | Short request phrases Borg can match against the current user request |
| `requires.bins` | no | List of CLI binaries that must all be on `$PATH` |
| `requires.env` | no | List of environment variables that must be set (also checks `[credentials]` store) |
| `requires.any_bins` | no | List of binaries where at least one must be present |
| `os` | no | List of OS constraints (e.g., `["macos"]`) |
| `install` | no | Install instructions per binary (keys: `brew`, `apt`, `npm`, `url`) |

### Body

The markdown body after the frontmatter contains instructions, command examples, and usage patterns. Write it as if you're teaching someone how to use the CLI tools -- the agent will follow these instructions literally.

### References

User skills can have `references/*.md` files in their directory. These are stored on the Skill struct but not auto-loaded into the system prompt context -- they can be read on demand.

### Scripts

User skills can have a `scripts/` directory. Script paths are stored on the Skill struct for use with `run_shell`.

## Requirement checking

At load time, each skill's requirements are validated:

- **Binaries**: checked via `which`
- **Environment variables**: checked via `std::env::var` and the `[credentials]` store
- **any_bins**: at least one binary from the list must be present
- **os**: current OS must match one of the listed values

If any requirement is missing, the skill is marked as unavailable. It still appears in `list_skills` output but won't have its full body injected into the system prompt.

## Token budgeting

Skills use progressive loading with a configurable token budget (`skills.max_context_tokens`, default 4000):

1. **Metadata** (name + description + status, plus triggers when present) is always loaded for all skills
2. **Full body** is loaded only for available skills, prioritizing the skills most relevant to the current user request

Configure the budget via settings:

```sh
borg settings set skills.enabled true
borg settings set skills.max_context_tokens 4000
```

## Creating a skill

The agent can create skills during a conversation using `apply_patch` with `target: skills`:

- Create a skill only when the workflow is reusable and non-trivial.
- Do not create a skill for one-off trivial tasks.
- Prefer a short body with a few concrete examples.

```
*** Begin Patch
*** Add File: my-api/SKILL.md
+---
+name: my-api
+description: "Query the My API service for data."
+triggers:
+  - "query my api"
+requires:
+  bins: ["curl"]
+  env: ["MY_API_KEY"]
+---
+
+# My API
+
+Use curl to query the API:
+
+\`\`\`bash
+curl -H "Authorization: Bearer $MY_API_KEY" https://api.example.com/v1/query?q=SEARCH_TERM
+\`\`\`
*** End Patch
```

You can also create skill files manually in `~/.borg/skills/<name>/SKILL.md`.

## Mandatory skills

Some built-in skills are essential and cannot be disabled. Today that set is `scheduler` and `skill-creator`.
