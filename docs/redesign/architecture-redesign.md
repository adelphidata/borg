# Borg Architecture Redesign — v1

Status: design, in active rework. This is the v1 foundation doc.

This document describes the v1 redesign of Borg as a daemonized always-on agent platform with seamless mTLS, third-party-client ingress, and the existing evolution gameplay preserved. v2 (mobile + native GUI) and v3 (multi-tenancy + agency-PKI integration + Sentry-mode hardening) are scoped at the end and detailed in their own forward-looking docs.

---

## Scope

### v1 — daemon platform + third-party clients
Daemon-as-only-host, gRPC transport, TUI as a gRPC client, OpenAI-compat HTTP + native channel adapters + generic webhook + MCP server (last priority), seamless mTLS pairing, audit chain, evolution gameplay preserved untouched.

### v2 — mobile + native GUI
First-party iOS / Android app (capability node + chat client), custom macOS GUI (likely separate repo). Layer 3 capability set expands to camera/mic/biometric/push.

### v3 — multi-tenancy + Sentry-mode compliance
Multi-user (workspaces become first-class, per-user memory namespaces, principal-bound audit), agency-PKI verifiers (StaticX509 / VaultPki / StepCa), SCIM/LDAP-driven JML, FIPS 140-3 crypto, per-workspace classification + retention, Tier 3 approval prompts, M-of-N admin quorum, Shamir KEK escrow, NIST 800-53 crosswalk, `sentry-strict` cargo feature.

The v1 schema is forward-compatible to v3 — every event row carries `principal_id` from day one (defaulted to the single device-owner in v1) so v3 multi-tenancy is additive, not migrative. Workspace concept is **not** in v1 (Hermes-shaped: one global agent state per `BORG_HOME`); workspaces land in v3 alongside multi-tenancy where they earn their keep.

---

## v1 requirements

1. **Daemon-only agent host.** The TUI never runs the agent in-process. There is exactly one agent host: `borgd`. Everything else is a client.
2. **Always-on.** Runs as a service via launchd (macOS) or systemd (Linux). Mac mini, VM, VPS, or laptop deployment targets.
3. **Open to third-party clients.** OpenAI-compat HTTP + MCP + native channel adapters + generic webhook. None privileged over the others.
4. **Seamless mTLS.** Pairing is one tap. Keys live in OS keychain / Secure Enclave / TPM. No PEM files. K8s-style: cluster CA managed by the daemon, auto-rotation, no manual cert ceremony.
5. **Evolution gameplay preserved.** Existing ed25519-signed XP chain, archetype, vitals, mood, stage gates, heartbeat — all carry over untouched.

---

## The shape, in one paragraph

A single headless daemon (`borgd`) owns all agent state and is the only writer to storage. The TUI, third-party clients, and channel adapters are all clients of the daemon — none of them run an agent in-process. Code execution happens in the existing Seatbelt (macOS) / Bubblewrap (Linux) sandbox; container-tier sandboxing is opt-in. Authentication is per-device mTLS with the daemon as its own internal CA; pairing is PAKE-based and requires no manual key handling. Third-party clients connect via an OpenAI-compatible HTTP adapter and an MCP server; existing native channel integrations stay as Layer 5 adapters. Tailscale handles remote access; loopback handles local. Audit is hash-chained and tamper-evident.

---

## Layer 1 — Kernel (`borgd`)

### Process model
- One static Rust binary. Async on tokio.
- Runs as a service via launchd (macOS) or systemd (Linux).
- Single writer to storage. No other process opens the SQLite DB. Every state mutation goes through the daemon.

### Storage
| What | Where |
|---|---|
| Sessions, messages, settings, devices, audit, evolution chain | **SQLite** (one file at `$BORG_HOME/borg.db`) |
| Media, attachments, large tool outputs | **On-disk blobs** in `$BORG_HOME/blobs/` |
| Provider API keys, channel webhook secrets, OAuth tokens | **Existing `[credentials]` table** (carried over from current Borg) |
| mTLS keys (server cert key, paired device cert keys) | **OS keychain / Secure Enclave / TPM** — never on disk |

`BORG_HOME` defaults to `~/.borg/`. To run a second isolated instance, set a different `BORG_HOME` and start a second daemon. Hermes-shaped — no in-binary workspace concept.

**No separate vault subsystem in v1.** The Layer 4e age-encrypted vault from prior drafts is cut. K8s analog: K8s itself doesn't have a vault for cluster credentials — it has the kubelet keystore for PKI and `Secret` resources for application secrets. Borg's v1 follows the same split: OS keychain for mTLS keys, `[credentials]` table for application secrets. A standalone vault subsystem only earns its keep when per-workspace scoping exists, which is v3.

### The resumable event log
Per-session, monotonic cursor. Every event lands here:
- Token deltas, tool-call starts, tool-call results
- Capability invocations and responses
- Audit entries
- Cancellation events
- Memory writes, settings changes, system events

Clients subscribe with a cursor and replay missed events on reconnect. **This is the load-bearing primitive.** Multi-client live view, audit trail, and crash recovery all derive from it.

### Portability
A `BORG_HOME` is a directory containing SQLite, blobs, audit log, and config. Move the directory → move the agent. No external services required.

---

## Layer 2 — Internal transport: gRPC over HTTP/2

### Why gRPC
- **Schemas with codegen.** Versioned protobuf definitions; clients generated for Rust (TUI), TypeScript (future web/GUI), Swift/Kotlin (v2 mobile).
- **Bidirectional streams** native to the protocol — needed for capability invocation and resumable token streaming.
- **Mature tooling**: auth interceptors, deadlines, cancellation, backpressure all standard.
- **gRPC-Web** for browser clients without a separate HTTP API.

### v1 schema scope
The v1 gRPC schema covers exactly: chat / tool calls / capabilities / event subscription / device pairing. It does **not** cover Canvas-style shared docs, voice, screen sharing, or embedded UI components — those are v2 schema additions, not v1 surface. This is a versioning policy: v1 clients work against v1 daemons forever; v2 daemons accept v1 clients with reduced feature set.

---

## Layer 3 — The capability protocol

Every connected client advertises the capabilities it can fulfill on connect. The daemon's tool registry includes capability-tools that route to the best-available client.

### v1 capability set
Scoped to what v1 clients (TUI, plugins, future web) actually offer:

```
terminal.exec      — run a shell command on the client's host (TUI sshed in)
file.pick          — pick a file from the client's host filesystem
clipboard.read     — read the client's clipboard
clipboard.write    — write the client's clipboard
host.run_shell     — run shell on the daemon's host (the default for run_shell)
host.read_file     — read a file on the daemon's host
```

The TUI advertises `terminal.exec`, `file.pick`, `clipboard.*` for its host. When the user is sshed into a server and runs the TUI on their laptop, the TUI offers `file.pick` against their laptop's filesystem. The agent on the daemon routes the call to the TUI. This fixes the "tools always run on the daemon's machine" pain.

### v2 expansion
`camera.capture`, `mic.record`, `location.get`, `biometric.confirm`, `push.notify`, `tts.speak`, `stt.listen` are reserved capability names. v1 clients don't advertise them; v2 mobile is where they earn their keep. Reserving the names now keeps the routing schema stable.

---

## Layer 4 — Security model

### 4a. Authentication — seamless mTLS

Pairing is one user action. No PEM files, no openssl ceremony, no manual key copying. K8s-shaped:

| Surface | Mechanism |
|---|---|
| Loopback (TUI + daemon on same host) | Optional auth (default: no auth on `127.0.0.1`); `min_key_storage` not enforced. |
| Tailnet | Tailscale identity headers verified via `tailscale whois`. |
| Paired remote devices (TUI on another laptop, future mobile/GUI) | **mTLS with daemon-issued certs.** Daemon runs an internal CA. Pairing flow (QR or 8-char base32 code) issues a cert with a device label. Keys live in the device's OS keychain / Secure Enclave / TPM. Auto-renewal silent at 14-day-to-expiry. Revoke = revoke cert; live connections drop within 5s. |
| Public TOFU listener (opt-in) | Same mTLS as paired devices, but the listener is gated behind an explicit confirmation in `/settings → Network`. |
| Channels (Telegram, Slack, etc.) | Webhook signature verification on the channel layer, no mTLS retrofit. |

The full mTLS + PAKE + listener / verifier model is in [`auth-and-onboarding.md`](auth-and-onboarding.md). v1 only ships TOFU and Tailscale verifiers — agency-PKI verifiers (StaticX509 / VaultPki / StepCa) are v3.

### 4b. Code execution sandbox
- **Default: Seatbelt (macOS) / Bubblewrap (Linux)** — inherited from current Borg, single binary, no extra runtime dependency. Catastrophic-command denylist applies (rm -rf /, mkfs, dd, curl|sh).
- **Opt-in: container or VM** — `/settings → Security → Sandbox → Container | VM`. Only enabled when the user opts in; v1 doesn't require Docker.

### 4c. Tool tiers (audit categorization, no prompts)

The tier system exists in v1 *only* for audit categorization — it does not gate execution.

| Tier | Examples | Behavior |
|---|---|---|
| **Tier 0 — read** | `read_file`, `list_dir`, `web_fetch`, `memory_search` | Auto. |
| **Tier 1 — write-local** | `apply_patch`, `write_memory` | Auto. Audited. |
| **Tier 2 — execute** | `run_shell` inside sandbox | Auto. Audited. Catastrophic denylist enforced. |
| **Tier 3 — privileged** | Host fs outside `BORG_HOME`, network egress changes, credential-store mutation | Auto. **High-priority audit event.** Surfaces in `/status` retroactively, never blocks. |

The "agent just executes / no approval prompts" invariant from `CLAUDE.md` is preserved verbatim. Tier 3 prompting (Sentry) is a v3 feature, not v1.

### 4d. Append-only audit log
- Hash-chained per session (and a daemon-wide system chain).
- Every tool call, every capability invocation, every auth event.
- Periodic Merkle root committed to a separate log file. Tampering is detectable.
- `borg audit verify` rebuilds and checks the chain. Run nightly via the existing self-healing path.
- **On by default in v1.** Cost is near-zero perf, gain is tamper-evident logs.

### 4e. Plugins
v1 keeps the current Borg model untouched: marketplace catalog + installer in `crates/plugins/`, plus the skills system. Manifest-first install with declared capabilities. Skill SHA-256 audit (V39) carries over. Sentry hardening (signed manifests, admin allowlist, quorum-gated, WASM runtime gate, `sentry-strict` build) is v3.

---

## Layer 5 — External adapters (ingress)

The kernel knows nothing about these. They are gRPC clients translating an external protocol to internal calls.

| Adapter | Protocol | v1 priority |
|---|---|---|
| **OpenAI-compat HTTP** | `/v1/chat/completions`, `/v1/responses`, `/v1/models` | **Highest.** Single best ROI — unlocks Open WebUI, ChatBox, LobeChat, AnythingLLM, OpenAI iOS app, Raycast. |
| **Channel adapters** | Each platform's webhook/API | Port the existing 8 from current Borg (Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage). Mostly mechanical. |
| **Generic webhook** | JSON in/out | Trivial. Ship alongside OpenAI-compat. |
| **MCP server** | Model Context Protocol | Lowest priority. Can slip to end of v1 release window. |

Adapters never read SQLite directly. They go through the kernel via gRPC. The kernel can revoke an adapter's auth without coordinating with anything else.

---

## Layer 6 — TUI (gRPC client)

Same UX as today's TUI, but talks to the daemon over gRPC. Crashing the agent doesn't crash the TUI.

The TUI is **never** an in-process agent host. There is exactly one agent host: the kernel. This is the single biggest behavioral change from current Borg, and the migration risk: every load-bearing TUI invariant from `CLAUDE.md` (mouse text-selection, scroll routing, composer history) must carry forward intact. See "Migration" below.

Web UI is **off by default** and not in v1 scope. v2 may ship a separate macOS native GUI (likely a separate repo).

---

## Layer 7 — Always-on operations

- **Service install:** `borg service install` writes launchd/systemd units, sets up log rotation, configures auto-start.
- **Health endpoint** at `/health/detailed` with structured status (auth, capabilities, audit chain integrity, channel connectivity).
- **Self-healing daily task** — extended to cover audit verification and certificate rotation reminders.
- **Crash recovery** via the resumable event log: any in-flight tool call is logged on start, re-checked on restart, marked failed if not recoverable.
- **Update mechanism:** `borg update` pulls a new binary, verifies signature, restarts the service.

---

## Migration — clean rewrite

v1 is a clean rewrite, not a phased evolution. The shape is too different from current Borg's in-process architecture for incremental migration to be cheaper than a rebuild.

### Plan
1. **Audit current invariants.** Before the rewrite, grep `CLAUDE.md` and the codebase for every "regressed multiple times" / "treat as a hard invariant" / guard-test comment. Carry those forward as test-first targets in the new TUI. The mouse-selection invariant in particular needs explicit forward-port plus the same source-level guard tests.
2. **Daemon scaffolding.** Build the kernel + gRPC transport + storage layer with the existing SQLite migrations carried over.
3. **Port the agent loop.** Lift `crates/core/src/agent.rs` into the daemon. The hook system, evolution events, vitals, heartbeat all live there.
4. **Port the TUI as a gRPC client.** Re-implement cell rendering, input handling, slash commands against the gRPC stream. This is the riskiest part.
5. **Port channels.** The existing 8 channel adapters become Layer 5 adapters (in-daemon translators, not out-of-process clients in v1).
6. **Add new ingress.** OpenAI-compat HTTP, generic webhook, MCP.
7. **Cut over.** Delete in-process TUI/agent code path. Keep `legacy/` directory as a side-by-side reference, then delete.

### Cost
Current Borg is frozen during the rewrite — no new features, no new channel adapters, no bug fixes for existing single-user use. Acceptable because the user count is one.

### Risk
Clean rewrites are where load-bearing-but-undocumented invariants get re-violated. The mouse-selection guard tests in `crates/cli/src/tui/mod.rs` and `app.rs` are the prototype — anything that pattern-matches them gets the same forward-port treatment.

---

## Honest tradeoffs

- **Latency.** Every tool call crosses gRPC. On loopback it's milliseconds. Streaming token deltas at terminal cadence is fine.
- **Install complexity.** "Run one binary" becomes "install the service, pair your devices, configure your tokens." More steps. Worth it for always-on.
- **Two protocols to maintain** — the internal gRPC schema and the OpenAI-compat external API. Codegen reduces the cost.
- **Clean rewrite cost.** Current Borg frozen during the rewrite.

---

## What I'd consciously *not* do in v1

- **No workspaces.** Hermes-shaped: one global agent state per `BORG_HOME`. v3 adds workspaces alongside multi-tenancy.
- **No vault subsystem.** OS keychain for mTLS keys + existing `[credentials]` table for app secrets. Same shape as K8s. v3 may add a vault for per-workspace scopes.
- **No mobile, no native GUI.** v2.
- **No Sentry mode.** No `--mode=sentry` flag, no FIPS, no SCIM, no quorum, no Derived PIV, no agency-PKI verifiers. v3.
- **No approval prompts.** "Agent just executes" invariant preserved. v3 (Sentry) adds Tier 3 prompts.
- **No web UI bundled.** Third-party OpenAI-compat clients cover the chat surface for users who want a browser UI.
- **No Electron desktop app.** Ever.
- **No relay infrastructure of our own.** Tailscale or your own TLS — not a hosted service.

---

## References
- Companion: [`auth-and-onboarding.md`](auth-and-onboarding.md) — mTLS, pairing, listeners, verifiers
- Companion: [`security-posture.md`](security-posture.md) — 3-state security multiplier in the evolution chain
- Companion: [`threat-model.md`](threat-model.md) — v1 adversary set
- Companion: [`llm-providers.md`](llm-providers.md) — provider categories, token accounting
- Companion: [`approvals.md`](approvals.md) — "agent just executes" + budgets + denylist
- Companion: [`data-handling.md`](data-handling.md) — retention + tombstone-on-purge + backup
- Companion: [`mobile.md`](mobile.md) — v2 placeholder
- Current Borg gateway: `crates/gateway/src/server.rs`
- Current daemon entry: `crates/cli/src/service/mod.rs::run_daemon`
- Hermes reference: `reference/hermes-agent/`
