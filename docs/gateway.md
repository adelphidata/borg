# Gateway

Borg's gateway is the HTTP ingress layer for channel integrations and
OpenAI-compatible clients. In production it is started by `borgd` under the
daemon supervisor, shares the daemon's session executor, and routes inbound
messages into the same session registry used by the TUI and gRPC clients.

The gateway is configured from DB-backed settings under `gateway.*`. The
default listener is `127.0.0.1:7842`; expose it beyond loopback only through a
trusted reverse proxy or one of Borg's tunnel providers.

## Runtime Model

- `borgd` starts the gateway from `crates/borgd/src/orchestrator.rs`.
- `GatewayServer` builds native channel clients, script-channel registries,
  health state, rate limiting, and HTTP routers in `crates/gateway/src/server.rs`.
- Agent turns are dispatched through `gateway::SessionExecutor`, implemented by
  `borgd::session::DaemonSessionExecutor`. The gateway must not construct agent
  backends directly in production.
- Native webhooks are resolved before script channels. If a native channel and a
  script channel share the same name, the native integration owns
  `/webhook/{name}`.
- Script channels live under `~/.borg/channels/<name>/channel.toml` and execute
  their configured scripts through the gateway executor.

## HTTP Surfaces

| Route | Purpose | Auth boundary |
|---|---|---|
| `POST /webhook/{name}` | Native provider webhooks and script channels | Provider signature/token for native channels; script channels own validation |
| `POST /webhook/slack/command` | Slack slash commands | Slack signing secret when configured |
| `POST /v1/chat/completions` | OpenAI-compatible chat completions | `Authorization: Bearer <client-token>` |
| `POST /v1/embeddings` | OpenAI-compatible embeddings | `Authorization: Bearer <client-token>` |
| `GET /v1/models` | OpenAI-compatible model aliases | `Authorization: Bearer <client-token>` |
| `POST /v1/webhook/{name}` | Generic signed webhook definitions | `x-borg-signature` + `x-borg-timestamp` HMAC |
| `POST /mcp` | MCP HTTP transport for read-only tools | `Authorization: Bearer <client-token>` |
| `GET /health`, `/healthz` | Liveness probe | Public on the gateway listener |
| `GET /readyz` | DB/provider/channel readiness | Public on the gateway listener |
| `GET /health/channels`, `/channels` | Channel health and listing | Public on the gateway listener |
| `POST /internal/poke`, `/internal/restart`, `/internal/cancel`, `/internal/away`, `/internal/available` | Local control hooks | Loopback-only peer address |

The OpenAI-compatible and MCP routes use client tokens managed by
`borg client-token ...`. Generic webhooks use `borg webhook ...` to create and
revoke named HMAC definitions.

## Native Channels

Native channels compile provider-specific parsing, verification, delivery, and
deduplication into the binary. Current native providers include Telegram, Slack,
Discord, Teams, Google Chat, Twilio, Signal, and iMessage support where the
platform allows it.

Provider docs under `docs/providers/` cover credential setup and provider-side
webhook URLs. In general:

- Telegram uses webhook mode when `gateway.public_url` is set and long-polling
  when it is not.
- Slack and Discord verify provider signatures before parsing.
- Twilio verifies request signatures when an auth token and public URL are
  configured.
- Signal and iMessage use local/native polling paths instead of public webhook
  delivery.

Inbound native messages flow through `NativeChannel::handle_webhook`, then
through the per-session `SessionQueue`, then into the daemon session executor.
Responses are delivered by the channel's `send_response` implementation after
the agent turn finishes.

## Generic Webhooks

`POST /v1/webhook/{name}` is separate from legacy/script `/webhook/{name}`.
Each definition stores JSON paths for sender, optional thread, and body text.
Requests must be JSON and include:

- `x-borg-timestamp`: current Unix timestamp within the replay window.
- `x-borg-signature`: HMAC over the request body using the issued webhook
  secret.

Unknown and revoked webhook names return the same 404 shape so callers cannot
enumerate active definitions.

## OpenAI-Compatible And MCP HTTP

The gateway exposes a small OpenAI-compatible surface for local tools such as
ChatBox or Open WebUI:

- `/v1/chat/completions` supports streaming and non-streaming chat responses.
- `/v1/embeddings` forwards embedding requests to the configured embedding
  provider.
- `/v1/models` returns stable aliases such as `borg-default` and `borg-fast`.

`POST /mcp` exposes the read-only MCP tool set. Mutating Borg tools must not be
listed or callable through MCP HTTP.

Both surfaces authenticate with Borg client tokens and rate-limit through the
shared ingress gate.

## Gateway Versus gRPC

The gateway is HTTP ingress for external services. The daemon gRPC API is a
separate control/data plane:

- Local CLI/TUI clients use the `~/.borg/run/borgd.sock` Unix socket. Filesystem
  permissions provide isolation.
- Optional TCP gRPC listeners require mTLS. Clients must pair first and present
  a CA-signed client certificate.
- Any TCP `tonic::Server` must use `build_server_tls_config`; authenticated
  services must be wrapped with `auth_interceptor` built from `build_verifier`.

Do not treat gateway bearer tokens or webhook secrets as substitutes for gRPC
mTLS. They protect different surfaces.

## Operations

- `gateway.host` and `gateway.port` choose the bind address.
- `gateway.public_url` tells providers where to send webhooks and is required
  for provider flows that cannot poll.
- `gateway.rate_limit_per_minute` limits external clients; loopback requests are
  exempt.
- Tunnel providers are managed through `borg tunnel ...`; see
  `docs/networking.md` for provider-specific runbooks.
- Health endpoints are intentionally lightweight. They are useful for probes but
  do not prove provider credentials are valid end to end.

When changing gateway routing, keep the existing ingress boundaries intact:
native provider verification stays in the native channel, generic webhooks stay
on `/v1/webhook/{name}`, OpenAI/MCP stay bearer-authenticated, and internal
control routes stay loopback-only.
