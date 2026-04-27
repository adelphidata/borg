# Task 03 — Layer 5 ingress adapters

> Read first: [`architecture-redesign.md`](../architecture-redesign.md) (Layer 5).
> Depends on: Task 01 (daemon hosts adapters); Task 02 (audit chain) is a soft dependency for the audit-write step.

## Status — Mostly landed

All three v1 HTTP surfaces (OpenAI-compat, generic signed webhook, MCP HTTP) are now mounted on the live gateway HTTP app via `.merge()` in `GatewayServer::run`, with production trait impls bridging admitted ingress to the daemon's session executor. The native channels (webhook + pull) were already wired. End-to-end coverage lives at `crates/borgd/tests/{openai_compat,generic_webhook,mcp_http}_e2e.rs` — each test boots the real `GatewayServer` against a free TCP port and a stub session executor, then issues live HTTP requests.

Production trait impls (added in this iteration):

- `gateway::openai_compat::AgentDispatcher` (`agent_dispatcher.rs`) — implements `ChatDispatcher`. Resolves the daemon's `SessionExecutor` (explicit handle preferred, falls back to the process-global one), calls `start_turn`, translates `AgentEvent::{TextDelta, ToolExecuting, TurnComplete, Error}` into `AgentEventChunk`, and ensures every stream terminates with `Done` or `Error` so SSE responses always close.
- `gateway::generic_webhook::AgentSink` (`agent_sink.rs`) — implements `AdmitSink`. Webhooks are fire-and-forget (route returns 202 before the agent finishes), so the sink spawns a detached drain task and logs errors via `tracing` rather than blocking the HTTP response.
- `gateway::mcp::build_read_only_tools` (`mcp/handlers.rs`) — implements `ReadOnlyToolHandler` for the v1 read-only tool surface (`memory_search`, `read_memory`, `web_fetch`, `web_search`, `list`) by adapting the existing `cortex::tool_handlers::*` functions. `list` rejects `agents` as `invalid_params` (-32602) because the multi-agent control surface is not exposed over MCP.

Mounting (in `crates/gateway/src/server.rs`): three private `build_*_router` helpers construct each sub-router from the live `Config` and merge them into the gateway's main `axum::Router`. A shared `db_open_factory()` closure and `build_ingress_gate()` helper keep the per-surface state construction consistent.

The gateway crate stays as a daemon-internal sub-library spawned by `crates/borgd/src/orchestrator.rs` (`spawn_gateway`). The user's stated direction is to keep the crate name `gateway` and consolidate inward rather than dissolving its source tree into `borgd`.

| Surface | Status | Path |
|---------|--------|------|
| Normalized `IngressMessage` | Shipped | `crates/cortex/src/ingress/message.rs` |
| Telegram, Slack, Discord, Teams, Google Chat, Twilio webhooks | Shipped (`NativeChannel` impls + registry, mounted at `/webhook/{name}`) | `crates/gateway/src/{telegram,slack,discord,teams,google_chat,twilio}/channel.rs` |
| Signal SSE pull adapter | Shipped (uses `signal::sse::run_sse_loop` — pull, not webhook) | `crates/gateway/src/signal/`, wired in `crates/gateway/src/server.rs:436-490` |
| iMessage pull adapter | Shipped, macOS-only (chat.db monitor → `handler::handle_polled_message`) | `crates/gateway/src/imessage/`, `monitor.rs:148` |
| OpenAI-compat HTTP (`/v1/chat/completions`, `/v1/models`) | Shipped — production `AgentDispatcher`, mounted, e2e-tested | `crates/gateway/src/openai_compat/`; e2e: `crates/borgd/tests/openai_compat_e2e.rs` |
| Generic signed webhook (`POST /v1/webhook/{name}`, HMAC + 5-min replay window + size cap) | Shipped — production `AgentSink`, mounted, e2e-tested | `crates/gateway/src/generic_webhook/`; e2e: `crates/borgd/tests/generic_webhook_e2e.rs` |
| MCP server stdio (`borg mcp stdio`, read-only tool surface) | Shipped | `crates/gateway/src/mcp/stdio.rs` |
| MCP server HTTP (`POST /mcp`, bearer-auth) | Shipped — production tool handlers via `build_read_only_tools`, mounted, e2e-tested | `crates/gateway/src/mcp/routes.rs`; e2e: `crates/borgd/tests/mcp_http_e2e.rs` |
| `borg client-token issue/list/revoke` (bearer auth for OpenAI-compat + MCP) | Shipped | `crates/interface/src/commands/client_token.rs` |
| Sender pairing / `dm_policy` / gateway bindings preserved | Shipped | `crates/cortex/src/pairing.rs`, `crates/cortex/src/config/gateway.rs` |

## Goal

All ingress (anything that injects user input into the agent) lives in the daemon as Layer 5 adapters that share one normalized message schema. v1 has four adapter families:

1. **Native channel adapters** — the existing 8 (Telegram, Slack, Discord, Teams, Google Chat, Twilio, Signal, iMessage). Six are webhook-driven and implement `NativeChannel`; Signal is SSE-pull from `signal-cli`; iMessage is a `chat.db` monitor on macOS.
2. **OpenAI-compat HTTP** — `/v1/chat/completions`, `/v1/models`. Lets ChatBox / Open WebUI / LobeChat / any OpenAI client point at Borg.
3. **Generic webhook** — single signed-payload endpoint that maps to the normalized schema. Lets users wire arbitrary upstream events.
4. **MCP server** — Borg exposes a read-only subset of its tools/skills as an MCP server.

All four sit behind the gateway server inside the daemon. None of them get a separate process.

## Normalized ingress message

The shipped shape (source of truth: `crates/cortex/src/ingress/message.rs`):

```rust
pub enum IngressSource {
    Channel(String),  // native channel name ("telegram", "slack", "signal", "imessage", …)
    Http(String),     // OpenAI-compat client identifier (token label)
    Webhook(String),  // webhook definition name
    Mcp(String),      // MCP client identifier
}

pub struct IngressMessage {
    pub source: IngressSource,
    pub principal: PrincipalId,        // "local" in v1; mTLS-derived in Task 02
    pub sender_id: String,             // platform-native, namespaced by source
    pub thread_id: Option<String>,     // platform-native thread (Slack thread_ts, Teams reply_to_id, …)
    pub body: MessageBody,             // text + attachments
    pub meta: serde_json::Value,       // raw provider payload for audit
    pub received_at: i64,              // Unix seconds, set at adapter entry
}
```

Every adapter parses its protocol and emits this. Downstream session resolution uses `cortex::ingress::session_key(sender_id, thread_id)`, and gateway-binding routing (provider/model/identity per channel/sender) is shared code.

## Webhook-based native channels (shipped)

Modules under `crates/gateway/src/{telegram,slack,discord,teams,google_chat,twilio}/` each export a `*Channel` struct implementing the `NativeChannel` trait (`crates/gateway/src/channel_trait.rs`). The trait carries:

- `handle_webhook` — verify signature, dedupe, parse to `InboundMessage`, compute session key.
- `send_response` — deliver agent output back to the platform.
- `bot_mention` and `start_typing` — optional UX hooks.

The unified dispatcher (`channel_trait::dispatch_webhook`) handles enqueue → agent invoke → delivery-queue persistence → at-least-once retry, so each channel module only owns protocol details. Existing `[[gateway.bindings]]` config and `gateway.channel_policies` semantics are preserved unchanged. Sender pairing (`dm_policy`: pairing/open/disabled) and thread-scoped history both keep working.

## Pull-based native channels (shipped)

Two channels do not fit the webhook trait by design — they pull messages instead of receiving HTTP POSTs:

- **Signal** — `signal::sse::run_sse_loop` subscribes to a `signal-cli` SSE stream; messages parse via `signal::parse` directly into `InboundMessage` and dispatch through `handler::invoke_agent`. Wired in `crates/gateway/src/server.rs:436-490`.
- **iMessage** (`#[cfg(target_os = "macos")]`) — `imessage::monitor::spawn_monitor` polls `~/Library/Messages/chat.db`, runs each new row through echo and reflection guards, then calls `handler::handle_polled_message`. Outbound is `osascript` via `imessage::send`.

Both still emit `IngressMessage` shape downstream (via the same handler path) and honor sender pairing + gateway bindings. They aren't part of the `NativeChannel` registry because there's no webhook to dispatch.

## OpenAI-compat HTTP (shipped)

Lives at `crates/gateway/src/openai_compat/`, mounted by `gateway::server` under the daemon. Surface:

- `POST /v1/chat/completions` (stream + non-stream paths).
- `GET /v1/models` — returns configured Borg gateway bindings (`borg-default`, `borg-fast`, …) as OpenAI-shaped model rows.
- Auth via `Authorization: Bearer <token>` against `cortex::auth::tokens` (tokens minted by `borg client-token issue`, stored hashed). Direct mTLS will be added when Task 02 lands.
- `/v1/models` advertises a fixed alias set (`borg-default`, `borg-fast`) hardcoded in `gateway::server::build_openai_compat_router`. The agent resolves the actual provider/model per its own config — the alias is decorative for client UIs (ChatBox, Open WebUI). If you want this driven by `gateway.bindings` later, that's the swap site.
- Each request normalizes into an `IngressMessage` with `IngressSource::Http(<token-label>)`, gated through `cortex::ingress::IngressGate` (rate-limit + sanitization), and dispatched via the `ChatDispatcher` trait. Production wires `AgentDispatcher`; tests inject a fake dispatcher.
- Tool-call passthrough: agent `ToolExecuting` events surface as OpenAI `tool_calls` deltas in the streaming response. Client-side `role=tool` results are not yet looped back into the agent's tool loop — clients that want agent-driven tool execution should let the agent run tools server-side.

Internal coverage: unit tests in `crates/gateway/src/openai_compat/{routes,dispatcher,types}.rs` exercise the streaming + non-streaming paths against a fake dispatcher.

**Remaining gate (deferred):** an end-to-end smoke against the official `openai` Python SDK is desirable but not required to ship — the trait-level fake-dispatcher tests exercise the same JSON surface that the SDK consumes. Track as an open item if a regression ever motivates it.

## Generic webhook (shipped)

Endpoint `POST /webhook/{name}` per row in the `webhook_definitions` table (managed via `borg webhook create/list/revoke`).

- Authentication: HMAC-SHA256 over `{ts}.{body}`, presented as `X-Borg-Signature: v1=<hex>` and `X-Borg-Timestamp: <unix-seconds>`. Replay window 5 min. Bodies above the size cap are 413'd before HMAC verification (load-shed defense — HMAC over very large bodies is CPU-bound).
- Anti-enumeration: unknown names return 404 only after the path lookup, with the same 200/401/404 timing profile as the other channels.
- A user-provided JSONPath/template extracts `sender_id`, `thread_id`, and `body` from the upstream payload (stored on the `webhook_definitions` row).
- Dispatched through the same agent pipeline as native channels — gateway bindings apply.

Internal coverage: `crates/gateway/src/generic_webhook/routes.rs:340+` covers replay-outside-window 401, anti-enumeration 404, and the size-cap 413.

## MCP server (shipped)

Modules under `crates/gateway/src/mcp/`:

- `routes.rs` — HTTP transport at `POST /mcp`, bearer-auth (same token scheme as OpenAI-compat).
- `stdio.rs` — `borg mcp stdio` subcommand for tools that prefer stdio transport (Claude Desktop, MCP inspector).
- `server.rs` — protocol handler.

Exposed read-only tools (5): `memory_search`, `read_memory`, `web_fetch`, `web_search`, `list`. The `list` tool reflects the daemon's resource catalog (tools/skills/channels/agents) which is read-only. **Skills are not exposed as MCP tools** — they're agent-internal instruction bundles, not callable surfaces.

Internal coverage: `tools_list_only_exposes_read_only_tools` (`server.rs:290`) is the architectural guard — it asserts the exact set of exposed tools, with a comment naming the invariant. Update both the list and that test together if any tool is added or removed.

**Hard limitation (documented, do not relax):** no agent-loop access from MCP — it's a tool surface, not a chat surface. Do not expose `apply_patch`, `run_shell`, or any T1+ tool over MCP in v1.

## Shared concerns

- All adapters share rate-limit middleware and prompt-injection sanitization (existing 5-layer defense from `cortex::ingress::IngressGate`).
- All adapters honor `principal_id` plumbing (defaulted to `local` in v1; cert-derived under Task 02).
- **Audit-chain write of inbound `IngressMessage` is deferred until Task 02 ships the hash chain.** Adapters already build the canonical `IngressMessage`, so the audit hook is one call site away.

## Out of scope

- Channel adapter additions beyond the existing 8.
- Web/mobile/native GUI clients (v2).
- Push notifications (v2 + v3).
- Dissolving `crates/gateway` into `crates/borgd` source tree (recommendation: keep as a daemon-internal sub-library; doc-only change to the redesign).

## Remaining gaps

- **Audit-chain inbound write** — blocked on Task 02. Once `audit_log_chain` exists, add a single hook at the adapter→agent boundary so every `IngressMessage` is recorded before the agent turn runs. The `IngressGate` constructed by `build_ingress_gate` currently uses `with_noop_audit`; that's the swap site.
- **OpenAI-compat tool-result loopback** — client `role=tool` results (the OpenAI-defined return path for tool execution) are not yet threaded back into the agent's tool loop. v1 clients that want agent-driven tool execution let the agent run tools server-side. Documented in `crates/gateway/src/openai_compat/mod.rs`.
- **Optional: official OpenAI Python SDK round-trip smoke** — desirable for confidence against client churn; not required to ship. The four `crates/borgd/tests/*_e2e.rs` cases exercise the same JSON wire format the SDK consumes.

## Done when

- All four adapter surfaces are reachable through `borgd` (✅ — native channels via `/webhook/{name}`, OpenAI-compat at `/v1/chat/completions` + `/v1/models`, generic webhook at `/v1/webhook/{name}`, MCP HTTP at `/mcp`).
- The 6 webhook-driven channels and 2 pull-driven channels keep their pre-rewrite behavior (✅ — internal channel test suite passes; no semantic drift).
- OpenAI-compat passes streaming + non-streaming tests, both at the route-layer with a fake dispatcher (`crates/gateway/src/openai_compat/routes.rs`) and end-to-end through the daemon with the real `AgentDispatcher` against a stub `SessionExecutor` (`crates/borgd/tests/openai_compat_e2e.rs`).
- Generic webhook rejects bad signatures, replays outside the window, and oversized bodies — covered at both the route layer and end-to-end through the daemon (`crates/borgd/tests/generic_webhook_e2e.rs`).
- MCP `tools/list` returns only the documented read-only tool set, enforced by `tools_list_only_exposes_read_only_tools` (route-layer) and re-asserted end-to-end in `crates/borgd/tests/mcp_http_e2e.rs` against `McpServer::MUTATING_TOOLS`.
- `crates/gateway` has no consumer outside `crates/borgd` (✅ — only `borgd::orchestrator` constructs `GatewayServer`).
- Audit-chain inbound write is wired (⏸️ deferred to Task 02).
- `crates/borgd/tests/*_e2e.rs` integration tests boot the gateway on a free TCP port and curl each surface to verify mounting + auth + dispatcher wiring (✅ — `openai_compat_e2e.rs`, `generic_webhook_e2e.rs`, `mcp_http_e2e.rs`).

## Risks to watch

- **Token sprawl.** OpenAI-compat bearer tokens are a credential type — store hashed, never logged, audit on issue/revoke. Already enforced by `client_token` storage path.
- **Channel regression.** The 8 channel adapters are user-visible behavior. Diff adapter behavior pre/post via the existing channel test suite; do not "improve" semantics.
- **MCP scope creep.** Tempting to expose `apply_patch` or `run_shell` over MCP; **don't** in v1 (that's the entire agent loop's authority over a fragile boundary). Read-only tools only — the guard test enforces this and must not be relaxed.
