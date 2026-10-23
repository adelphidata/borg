---
name: paperclip
description: "Orchestrate multi-agent autonomous companies via the Paperclip control plane (localhost:3100 REST API)"
requires:
  bins: ["npx", "curl"]
install:
  npx:
    brew: node
    apt: npm
    url: "https://nodejs.org"
  curl:
    brew: curl
    apt: curl
---

# Paperclip

Node.js control plane that orchestrates AI agents (Claude Code, Cursor, Bash,
HTTP) as an "autonomous company" — org charts, budgets, governance, goals,
activity logs. Server: `http://localhost:3100` with embedded PostgreSQL.

```bash
npx paperclipai onboard --yes                 # local, unauthenticated
# Also: --bind lan  (auth'd), --bind tailnet
```

## Auth

Bearer API keys from `agent_api_keys` (hashed, company-scoped — can't cross
companies). Export `PAPERCLIP_API_KEY`:

```bash
curl -H "Authorization: Bearer $PAPERCLIP_API_KEY" \
  http://localhost:3100/api/companies
```

## Mutation invariants

- Single-assignee tasks (one agent per task).
- Atomic checkout / exclusive issue locks → `409` on conflict; re-read state
  and retry.
- Governed actions need approval; budget hard-stops auto-pause.
- Every mutation writes an activity log entry.

`/api/health` is unauthenticated. 403 = bearer key doesn't own the target.

## Adapters & telemetry

External adapter registry: `~/.paperclip/adapter-plugins.json`.

Disable telemetry: `PAPERCLIP_TELEMETRY_DISABLED=1` or `DO_NOT_TRACK=1` or
`telemetry.enabled: false` in config. CI auto-disables.
