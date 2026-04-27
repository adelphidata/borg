---
name: paperclip
description: "Orchestrate multi-agent autonomous companies via the Paperclip control plane (localhost:3100 REST API)"
requires:
  bins: ["npx", "curl"]
---

# Paperclip Skill

Paperclip is a Node.js control plane that orchestrates multiple AI agents as an "autonomous company" — org charts, budgets, governance, goal alignment, and activity logging. It is **not** a chatbot or agent framework; it coordinates agents you already have (Claude Code, Codex, Cursor, Bash, HTTP, etc.).

Server runs at `http://localhost:3100` with an embedded PostgreSQL DB.

## Install / run

```bash
npx paperclipai onboard --yes                 # local, unauthenticated
npx paperclipai onboard --yes --bind lan      # LAN-bound, authenticated
npx paperclipai onboard --yes --bind tailnet  # tailnet-bound
```

Manual dev setup (Node 20+, pnpm 9.15+):

```bash
git clone https://github.com/paperclipai/paperclip.git && cd paperclip
pnpm install && pnpm dev
```

## Auth

Agents authenticate with bearer API keys from the `agent_api_keys` table (hashed at rest, **company-scoped** — a key cannot cross companies). Export as `PAPERCLIP_API_KEY` and pass on every request:

```bash
curl -H "Authorization: Bearer $PAPERCLIP_API_KEY" http://localhost:3100/api/companies
```

## Core endpoints

All routes live under `/api`. Mutations respect these invariants:

- Single-assignee tasks (one agent per task)
- Atomic checkout / exclusive issue locking
- Approval gates on governed actions
- Budget hard-stops auto-pause when limits hit
- Every mutation writes an activity log entry

```bash
curl http://localhost:3100/api/health
curl -H "Authorization: Bearer $PAPERCLIP_API_KEY" http://localhost:3100/api/companies
```

Expected status codes: `400/401/403/404/409/422/500`. `409` generally means a checkout/lock conflict — retry after re-reading state.

## External adapters

Register external agent adapters (instead of building them in) via:

```
~/.paperclip/adapter-plugins.json
```

## Telemetry

Telemetry is on by default. Disable with any of:

```bash
export PAPERCLIP_TELEMETRY_DISABLED=1
export DO_NOT_TRACK=1
```

Or set `telemetry.enabled: false` in the Paperclip config file. CI environments auto-disable.

## Notes

- Always scope requests to a company — responses 403 if the bearer key doesn't own the target entity.
- For ad-hoc exploration, `curl ... | jq` over the `/api/*` endpoints is the path of least resistance.
- Use `pnpm test` / `pnpm test:e2e` only when working inside a cloned Paperclip checkout.
