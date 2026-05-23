# MCP (Model Context Protocol)

Borg **consumes** MCP servers. The agent loop discovers tools, resources, and prompts on external MCP servers and exposes the tools to the LLM as first-class entries in the catalog. Borg does **not** expose itself as an MCP server from the agent runtime — the unrelated `borg mcp stdio` subcommand serves a strict read-only subset for the legacy Claude Desktop / MCP Inspector use case and is not part of this surface.

This document covers the consumer side: the `borg mcp` subcommand tree, the four transports we support, the security model, and the operator runbook.

## Quick start

```sh
# Smoke test — no credentials, exits cleanly if uvx and mcp-server-time are installed.
borg mcp add-preset time --preset time
borg mcp test time

# Enable MCP globally, then restart borgd so the agent picks the server up.
borg settings set mcp.enabled true
sudo systemctl restart borgd     # or: borg service restart
```

After restart, the LLM sees one new tool per discovered MCP tool, namespaced `mcp_<server>_<tool>`. For the `time` preset, that's `mcp_time_get_current_time`.

## Transports

All four protocols work. The transport is chosen by config, not by Borg.

| Transport | Wire format | When to use |
|---|---|---|
| **stdio** | Newline-delimited JSON-RPC over a child process's stdin/stdout | Local servers (`npx`, `uvx`, custom binaries). Default for community MCP servers. |
| **streamable-http** | Single HTTPS endpoint; POST for client → server, optional `text/event-stream` chunked response for server → client streaming. `Mcp-Session-Id` makes the session resumable. | Remote servers (`https://mcp.notion.com/mcp`, vendor SaaS). |
| **http+sse** (legacy) | Separate `GET /sse` event stream + `POST /messages` for client replies | Older servers still on the pre-streamable spec. |
| **HTTP/2** | Negotiated automatically when the server advertises `h2` via TLS ALPN | Not a separate transport — `streamable-http` and `http+sse` both upgrade transparently when the server supports it. |

Every transport implements the same `mcp::transport::Transport` trait and is constructed by `mcp::transport::open(&manifest, policy)`. Adding a future transport (e.g. WebSocket) means implementing the trait and adding a variant to `mcp::manifest::Transport`.

## CLI reference

```sh
borg mcp list                              # show configured servers + status
borg mcp add <id> --stdio <cmd> [-- args]  # register a stdio server
borg mcp add <id> --http <url>             # register a streamable-HTTP server
borg mcp add <id> --sse  <url>             # register a legacy SSE server
borg mcp add-preset <id> --preset <name>   # register from a built-in preset
borg mcp enable <id>                       # mark enabled (default on add)
borg mcp disable <id>                      # hide its tools from the LLM
borg mcp remove <id>                       # delete the config entry
borg mcp test <id>                         # one-shot connect + tools/list, no daemon
```

Common flags on `add`:

| Flag | Default | Notes |
|---|---|---|
| `--env KEY=VAL` | none | stdio only; may repeat. Values may contain `${SECRET:NAME}` |
| `--header K=V` | none | http/sse only; may repeat. May contain `${SECRET:NAME}` |
| `--auth bearer:TOKEN` | none | http/sse shortcut; mutually exclusive with literal `Authorization` header |
| `--connect-timeout-ms` | 60_000 | handshake deadline |
| `--call-timeout-ms` | 120_000 | per-`tools/call` deadline |
| `--rate-limit-per-minute` | 60 | steady-state cap |
| `--rate-limit-burst` | 10 | token-bucket burst capacity |
| `--max-tools` | 32 | per-server cap on tools registered |
| `--force` | off | overwrite an existing entry with the same id |

`borg mcp test` is intentionally standalone — it loads config, resolves secrets, opens the transport, runs `initialize` + `tools/list`, and exits. It does not require borgd to be running, so an operator can validate a freshly-added server before bouncing the daemon.

## Presets

```sh
borg mcp add-preset time       --preset time
borg mcp add-preset fs         --preset filesystem  --fs-root /path/to/scope
borg mcp add-preset github     --preset github      # then: borg settings set credentials.github_pat=ghp_…
borg mcp add-preset notion     --preset notion      # then: borg mcp add Notion bearer via --auth or headers
```

| Preset | Transport | Command / URL | Credential |
|---|---|---|---|
| `time` | stdio | `uvx mcp-server-time` | none |
| `filesystem` | stdio | `npx -y @modelcontextprotocol/server-filesystem <root>` | none (scoped path is the security boundary) |
| `github` | stdio | `npx -y @modelcontextprotocol/server-github` | `GITHUB_PERSONAL_ACCESS_TOKEN=${SECRET:github_pat}` |
| `notion` | streamable-http | `https://mcp.notion.com/mcp` | operator adds `--auth bearer:${SECRET:notion_token}` after creation |

Presets are templates — adding one writes a config row but does not download, install, or spawn anything. The first time the LLM actually calls a tool from that server, the underlying binary is fetched via `npx`/`uvx` (which themselves enforce signature checks per their own threat models). This staged install is intentional: an operator gets the chance to remove a preset before any third-party code executes.

The `filesystem` preset refuses `$HOME` as the root even when passed explicitly via `--fs-root`. Borg's `security.blocked_paths` defaults backstop sensitive subdirectories at the tool-call boundary, but starting from a dedicated subtree (`~/.borg/mcp/fs-scope` by default) is much less rope.

## Security model

MCP servers are treated as **untrusted code at arm's length**. The threat model assumes an MCP server may be actively hostile: trying to exfiltrate credentials, inject instructions into the agent, or read files outside its scope.

### Layer 1 — environment isolation (stdio)

Stdio children inherit only a hard-coded allowlist of env vars: `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`/`TMP`/`TEMP`, `LANG`/`LC_*`/`TZ`, `TERM`/`COLORTERM`, and the `XDG_*` family. **Every other env var is stripped** — including every `*_API_KEY`, `BORG_KEK_PASSPHRASE`, `AWS_*`, `GITHUB_TOKEN`, OAuth tokens, and the operator's shell history. Credentials reach a server only if the operator declares them explicitly via `--env KEY=VAL` (with a `${SECRET:NAME}` placeholder that resolves from the keychain at spawn time).

The allowlist lives in `crates/mcp/src/transport/mod.rs::ENV_ALLOWLIST` and is enforced before the child process is launched. The accompanying unit test pins the invariant: changes to the list must justify themselves against the no-credential-leak rule.

### Layer 2 — URL safety (HTTP / SSE)

`http://` URLs are refused unless `mcp.allow_plaintext_http=true`. URLs whose hostname resolves to loopback / RFC1918 / link-local are refused unless `mcp.allow_local_network=true`. DNS is resolved at check time and the resolved IPs are pinned into the reqwest connector, so a hostile authoritative DNS server cannot rebind between the policy check and the connect.

### Layer 3 — trust-boundary wrapping

Every MCP tool result is wrapped in `<mcp_tool_result server="<id>" trust="untrusted">…</mcp_tool_result>` before being added to the LLM context. This makes server output structurally distinguishable from operator or system text, so prompt-injection heuristics and downstream redaction can target the boundary.

### Layer 4 — secret redaction

Tool results pass through Borg's existing secret-redaction pipeline (the same one applied to channel/TUI/log sinks) before being wrapped. Known credential patterns (AWS, GitHub, Anthropic/OpenAI/Slack tokens, JWTs, DB connection strings, anything in the operator's keychain) are replaced with `***REDACTED***` and an audit row is written.

### Layer 5 — schema gauntlet

Each tool's `inputSchema` is validated at registration time. We reject schemas that:

- Exceed 32 KiB serialized (each schema rides in the system prompt every turn).
- Have more than 64 input properties.
- Contain `$ref` (would require resolving against external schema URLs, an exfil vector).
- Are not a JSON object at the root.
- Would push the total tool count past the per-server cap (`tools.max`, default 32) or the global cap (`mcp.max_tools_total`, default 128).

Rejections do not fail the whole server — the offending tool is dropped, the rest stay, and an audit event records the reason.

### Layer 6 — Plan-mode opt-in

Plan mode (`/plan`) blocks all MCP tools by default. The operator promotes a tool into Plan mode by listing it in the per-server `tools.read_only` config. The server's own `annotations.readOnlyHint` is **not** trusted for Plan mode — only the operator's explicit list. (The server's hint is still honored for parallel-fanout batching decisions in Execute mode, where the trust requirement is lower.)

### Layer 7 — rate limits

Each server has a token-bucket rate limit (`rate_limit_per_minute`, `rate_limit_burst`). When a server is exhausted, the dispatch fails closed and the LLM sees a clear "rate limited" error — refusal is never silent.

### Layer 8 — kill switches

| Knob | Effect |
|---|---|
| `mcp.enabled = false` | Skip all MCP init; tool catalog returns empty; dispatcher refuses MCP names |
| `mcp.disabled_servers = ["<id>", …]` | Hide a specific server's tools live (no daemon restart needed) |
| `mcp.max_tools_total` | Global ceiling on total MCP tool count (default 128) |
| `mcp.allow_plaintext_http` | Allow `http://` URLs (default false) |
| `mcp.allow_local_network` | Allow loopback / RFC1918 / link-local resolution (default false) |

A live `borg settings set mcp.disabled_servers=["bad-actor"]` removes that server's tools from the agent catalog on the next turn, even if a connected session is still open. Followed by `borg mcp remove bad-actor` to make it permanent.

## Audit & telemetry

Hash-chained audit events (via the same `AuditWriter` as the WASM plugin sandbox):

- `mcp_server_started` / `mcp_server_failed`
- `mcp_tool_registered` / `mcp_schema_rejected` (with `reason`)
- `mcp_tool_called` (server, tool, args_hash, duration, outcome)
- `mcp_rate_limited`
- `mcp_blocked_path_attempt`
- `mcp_response_truncated`

OTEL metrics:

- `borg.mcp.executes_total{server,tool,outcome}`
- `borg.mcp.execute_duration_ms{server,tool}`
- `borg.mcp.tools_listed_total{server}`
- `borg.mcp.rate_limited_total{server}`
- `borg.mcp.transport_errors_total{server,transport,kind}`

## Operator runbook

**A server is misbehaving (loud).** `borg settings set mcp.disabled_servers='["<id>"]'` — instant, no daemon restart. Then investigate via `~/.borg/logs/daemon.log` and the audit table.

**A server is misbehaving (quiet — schema flood).** Check the audit table for `mcp_schema_rejected`. If a server consistently emits oversized schemas it's either a bug or hostile; remove it.

**Credentials rotated.** Update the keychain via `borg settings set credentials.<name>=<new>`. Restart borgd to refresh long-lived sessions, or `borg mcp test <id>` to verify the new credential without restart.

**Tool name collision.** Borg refuses to register a tool whose namespaced name collides with a builtin or another MCP tool. Inspect with `borg mcp list`; rename the server via remove + re-add to break the conflict.

**Plan mode blocks a tool I trust.** Add the original tool name (pre-sanitization) to the server's `tools.read_only` list:

```sh
borg settings set 'mcp.servers.<id>.tools.read_only=["search","read_resource"]'
```

## Design notes

- **No OAuth (yet).** Phase 1 supports `Authorization: Bearer …` headers. OAuth 2.1 PKCE with dynamic client registration and loopback callback is sketched out in the source but not wired — it lights up when a real server demands it.
- **No sampling.** Server-initiated LLM calls (the `sampling` capability) are refused with `not_supported`. Letting an untrusted server steer Borg's LLM is exactly the wrong direction for the threat model.
- **No resources/prompts in the catalog.** Phase 1 surfaces `tools` only; resources and prompts are deferred. The protocol layer parses them; the cortex adapter does not yet expose them.

## See also

- `CLAUDE.md` → `### WASM Plugin Sandbox` for the analogous sandbox / kill-switch / rate-limit pattern that MCP mirrors.
- `crates/mcp/src/lib.rs` — crate-level docs and trust posture.
- `crates/cortex/src/mcp_clients.rs` — cortex-side adapter (init, dispatch, trust wrapping).
- `crates/cortex/src/config/mcp.rs` — config model and `${SECRET:NAME}` resolution.
