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

## TUI surface (read-only)

Inside the TUI, two slash commands surface MCP state without dropping to
a shell. Both are read-only: every mutation (add, remove, disable, test)
still flows through `borg mcp …` so the trust model — env allowlist,
DNS pinning, schema gauntlet — stays anchored in one place.

| Command | Effect |
|---|---|
| `/mcp` | Open a dedicated popup that lists every configured server with its transport (`stdio`/`http`/`sse`), lifecycle state (`connected`, `reconnecting`, `auth-failed`, `degraded`, `disabled`, `live-disabled`, `not-started`), and live tool count. |
| `/plugins` (alias: `/plugin`) | The unified plugins/skills/WASM popup also shows an `MCP SERVERS` section so operators see "everything that adds tools to the agent" in one view. |

`live-disabled` rows are the ones the operator has live-killed via
`borg settings set mcp.disabled_servers=…`; that state wins over the
per-server `enabled` flag in both popups so a just-applied kill switch
is visible without restarting borgd. The `/plugins` MCP section
intentionally does not accept Space-to-toggle — pressing Space on an
MCP row prints a pointer to `borg mcp disable <id>` instead. The
schema gauntlet only runs on the install path, so accepting toggles
inside a popup would be a quiet way to bypass it.

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

A result's `structuredContent` is part of that output. When the content blocks are empty it is rendered as compact JSON in their place; when the text is already the same JSON it is not repeated; otherwise it follows the text under a `[structured content]` line. It passes through the same size cap, redaction, and wrapper as the text.

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

Discovery itself is also bounded: `tools/list` follows `nextCursor` pagination (so multi-page catalogs aren't silently truncated to page one), but at most 100 pages / 1024 tools per server, cursors over 64 KiB or repeated cursors are rejected, and the whole pagination loop shares the per-call timeout. Violating any bound excludes the server for that discovery pass.

The per-server `tools.include` allowlist has three states, and they are not interchangeable: key absent means no allowlist (only `tools.exclude` applies); an explicit empty list `[]` means **no tools** — the server registers nothing and a warning names it; a non-empty list admits only those names (globs allowed). `borg mcp list` prints the resulting filter as `all tools`, `no tools`, `only: …`, or `all except: …`.

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
| `mcp.allow_plaintext_http` | Allow `http://` URLs (default false). Cloud-metadata FQDNs (`metadata.google.internal`, `metadata.goog`) stay refused even with this on |
| `mcp.allow_local_network` | Allow loopback / RFC1918 / link-local resolution (default false). Cloud-metadata FQDNs stay refused even with this on |
| `mcp.max_response_bytes` | Per-`tools/call` response size cap (default 1 MiB). Output beyond this is truncated before redaction/wrapping; an `mcp_response_truncated` audit row is written |

A live `borg settings set mcp.disabled_servers=["bad-actor"]` removes that server's tools from the agent catalog on the next turn, even if a connected session is still open. Followed by `borg mcp remove bad-actor` to make it permanent.

## Audit & telemetry

Hash-chained audit events (via the same `AuditWriter` as the WASM plugin sandbox). Constant names live in `crates/cortex/src/db/audit_writer.rs::audit_events::*`:

- `mcp_server_started` / `mcp_server_failed` (lifecycle: handshake success / terminal failure)
- `mcp_tool_registered` / `mcp_schema_rejected` (per-tool: accepted / rejected with `reason`)
- `mcp_tool_executed` (server, tool, success, duration_ms, threat_level, redaction_hits, [error])
- `mcp_rate_limited` (server, tool, retry_after_secs, limit_per_minute, burst)
- `mcp_response_truncated` (server, tool, original_bytes, truncated_to_bytes, cap_bytes) — fires when output exceeds `mcp.max_response_bytes`
- `mcp_sampling_refused` (server, method) — fires when the server sends an unsolicited `sampling/createMessage` (or any other server-initiated RPC) and the client replies with JSON-RPC `-32601`
- `mcp_blocked_path_attempt` — reserved for the filesystem-preset path-filter audit row (not yet emitted; preset filtering happens at the `fs_read`/`fs_write` boundary)

OTEL-style structured events under target `borg.mcp` (`tracing::info!` / `tracing::warn!`). The borgd tracing-opentelemetry layer projects these into:

- `borg.mcp.executes_total{server,tool,outcome}` — `event="execute"`, `outcome` ∈ `success`/`error`
- `borg.mcp.execute_duration_ms{server,tool}` — `duration_ms` field on the same `event="execute"` record
- `borg.mcp.tools_listed_total{server}` — `event="tools_listed"`, `count`
- `borg.mcp.rate_limited_total{server}` — `event="rate_limited"`, `retry_after_secs`
- `borg.mcp.transport_errors_total{server,transport,kind}` — `event="transport_error"`, `transport` ∈ `stdio`/`streamable-http`/`sse`, `kind` ∈ `open`/…

Off-daemon contexts (CLI one-shots, tests) read the same data straight out of the log stream.

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
- **No sampling — actively refused.** Server-initiated requests (`sampling/createMessage`, `roots/list`, anything else) are answered with JSON-RPC `-32601` (Method not found) inside `crates/mcp/src/session.rs`. The refusal is also drained by the cortex supervisor and surfaces as an `mcp_sampling_refused` audit row so an operator can SEE that a server tried to drive borg's LLM. Letting an untrusted server steer Borg's LLM is exactly the wrong direction for the threat model.
- **`Mcp-Session-Id` resumability** is supported by the streamable-HTTP transport. The transport captures the server-emitted `Mcp-Session-Id` on first response, echoes it on every subsequent POST, and refreshes it on rotation. A `404` means the server forgot the session (it restarted, or the TTL lapsed): the transport re-runs the `initialize` handshake and retries the request once, so a server restart costs nothing. A second `404` against the fresh session fails the call — that server is broken, and retrying would spin. A `404` when no session id was in play is reported as a wrong endpoint URL.
- **Reply framing is the server's choice.** A streamable-HTTP server may answer a POST with a JSON body or with a `text/event-stream` body; Borg reads both. On an event stream the read ends when the response to the request arrives, so a server that keeps the stream open does not stall the call. Notifications that arrive before the response are kept.
- **Transport size caps are per message.** The transport ceiling is 8 x `mcp.max_response_bytes`. It applies to one JSON body, one stdio line, or one event of an event stream (streamable-HTTP replies and the legacy SSE stream alike). An event stream has no total limit, so a long-lived stream of small events is normal. One oversized event fails the call on streamable-HTTP and closes the legacy SSE transport, which tears the session down with an `mcp_server_failed` row.
- **Idle liveness probe.** After 60 s with no traffic the session sends `ping`. Any reply, even an error, proves the server is alive. Some servers drop `ping` with no reply, so a `ping` timeout is confirmed with one `tools/list` before teardown; if that succeeds the session stays up and later idle probes use `tools/list` directly. A server that is silent on both, or whose transport is closed, is torn down (`mcp_server_failed`, `phase: runtime`) and comes back with `/reload mcp`.
- **Protocol version negotiation.** Borg declares `2025-06-18` in the `initialize` body and adopts whatever revision the server answers with, including one Borg does not know (a warning is logged). The agreed value is then sent as `MCP-Protocol-Version` on every later request, as the spec requires. The header always carries the *negotiated* value — a header and body that disagree get the request judged against rules the client never agreed to.
- **No resources/prompts in the catalog.** Phase 1 surfaces `tools` only; resources and prompts are deferred. The protocol layer parses them; the cortex adapter does not yet expose them.
- **Startup never gates the boot.** Cache-warmed stdio servers register their previous catalog instantly and re-handshake in the background. Cache-miss servers start concurrently and share one boot budget (`mcp.startup_grace_ms`, default 2 s); anything still starting when it expires continues in the background and registers its tools on completion. One unreachable server's 60 s connect timeout can no longer serialize in front of the daemon's boot.

## See also

- `AGENTS.md` → `### WASM Plugin Sandbox` for the analogous sandbox / kill-switch / rate-limit pattern that MCP mirrors.
- `crates/mcp/src/lib.rs` — crate-level docs and trust posture.
- `crates/cortex/src/mcp_clients.rs` — cortex-side adapter (init, dispatch, trust wrapping).
- `crates/cortex/src/config/mcp.rs` — config model and `${SECRET:NAME}` resolution.
