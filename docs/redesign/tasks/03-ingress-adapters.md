# Task 03 — Layer 5 ingress adapters

> Read first: [`architecture-redesign.md`](../architecture-redesign.md) (Layer 5).
> Depends on: Task 01 (daemon hosts adapters).

## Goal

Move all ingress (anything that injects user input into the agent) into the daemon as Layer 5 adapters that share one normalized message schema. v1 has four adapter families:

1. **Native channel adapters** — port the existing 8 (Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage) from `crates/gateway` into `borgd`.
2. **OpenAI-compat HTTP** — `/v1/chat/completions`, `/v1/models`. Lets ChatBox / Open WebUI / LobeChat / any OpenAI client point at Borg.
3. **Generic webhook** — single signed-payload endpoint that maps to the normalized schema. Lets users wire arbitrary upstream events.
4. **MCP server** — Borg exposes its own tools/skills as an MCP server. **Last priority** — can ship after the others if time pressed, but in v1.

All four sit behind the same gRPC-internal ingress fan-in inside the daemon. None of them get a separate process.

## Scope

### Normalized ingress message

```rust
struct IngressMessage {
    source: IngressSource,         // Channel(name) | Http(client_id) | Webhook(name) | Mcp(client_id)
    sender_id: String,             // platform-native id, namespaced by source
    thread_id: Option<String>,     // platform-native thread (Slack thread_ts, Teams reply_to_id, etc.)
    body: MessageBody,             // text + attachments
    meta: serde_json::Value,       // raw provider payload for audit
    received_at: i64,
}
```

Every adapter parses its protocol and emits this. Downstream session resolution (`{sender_id}:{thread_id}`) and gateway-binding routing (provider/model/identity per channel/sender) is shared code.

### Native channel port

- Move `crates/gateway/src/{telegram,slack,discord,teams,google_chat,signal,twilio,imessage}/` modules into `borgd`.
- Existing `[[gateway.bindings]]` config and `gateway.channel_policies` keep their semantics. No new config surface for v1.
- `iMessage` stays `#[cfg(target_os = "macos")]`.
- Sender pairing (`dm_policy`: pairing/open/disabled) keeps working.
- Thread-scoped history keeps working.

### OpenAI-compat HTTP

- New `crates/borg-openai-compat` module inside daemon.
- Endpoints: `POST /v1/chat/completions` (stream + non-stream), `GET /v1/models`.
- Auth: bearer token mapped to a paired-device cert, OR mTLS direct (clients that support it). Bearer tokens are minted by `borg client-token issue` and stored hashed.
- Maps OpenAI request → IngressMessage; streams agent output back as SSE in OpenAI's chunk format.
- Tool/function-calling pass-through: agent's tool calls are surfaced in the OpenAI `tool_calls` field; client tool results come back via the next request (matches OpenAI semantics).
- Model selection: client `model` field maps to a configured Borg gateway binding (e.g. `borg-default`, `borg-fast`). `GET /v1/models` returns those bindings.

### Generic webhook

- Single endpoint `POST /v1/webhook/{name}` per configured webhook.
- Payload signed with HMAC-SHA256 using a per-webhook secret (`X-Borg-Signature` header, timestamped, replay window 5 min).
- User-provided JSONPath/template extracts `sender_id`, `thread_id`, `body`. Stored as a `webhook_definitions` row.
- Same routing pipeline as channels (gateway bindings apply).

### MCP server (last priority)

- Expose Borg's tools (`memory_search`, `read_memory`, `web_fetch`, `web_search`) as an MCP server over stdio AND HTTP.
- Auth via the same bearer-token / mTLS path as OpenAI-compat.
- Skills are not exposed (they're agent-internal instruction bundles, not tools).
- Document the limitations explicitly: no agent-loop access from MCP — it's a tool surface, not a chat surface.

### Shared concerns

- All adapters share rate-limit middleware and prompt-injection sanitization (existing 5-layer defense).
- All adapters write incoming messages into the audit chain (Task 02) before dispatch.
- All adapters honor `principal_id` plumbing from Task 02.

## Out of scope

- Channel adapter additions beyond the existing 8.
- Web/mobile/native GUI clients (v2).
- Push notifications (v2 + v3).

## Done when

- A user with a single daemon can: receive Telegram/Slack messages exactly as they did pre-rewrite; point ChatBox at `https://borgd.local/v1/chat/completions` (with bearer token) and chat; POST a JSON payload to a configured webhook and have the agent respond on the originating channel; point Claude Desktop at the daemon as an MCP server and call `memory_search`.
- All 8 channel adapters' existing test coverage passes inside `borgd`.
- `crates/gateway` is either deleted or downgraded to a thin shim that re-exports daemon types (decide based on dependency graph; preference: delete).
- OpenAI-compat passes a smoke test against the official `openai` Python SDK (chat + streaming + tool_calls round-trip).
- Webhook signature verification rejects bad signatures and replays.
- MCP server passes the MCP inspector tool's basic conformance.

## Risks to watch

- **Token sprawl.** OpenAI-compat bearer tokens are a credential type — store hashed, never logged, audit on issue/revoke.
- **Channel regression.** The existing 8 channels are user-visible behavior. Diff the adapter behavior pre/post via the existing channel test suite; do not "improve" semantics.
- **MCP scope creep.** Tempting to expose `apply_patch` or `run_shell` over MCP; **don't** in v1 (that's the entire agent loop's authority over a fragile boundary). Read-only tools only.
