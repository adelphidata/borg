# Task 01 — Daemon kernel + gRPC + TUI client

> Read first: [`architecture-redesign.md`](../architecture-redesign.md) (Layers 1–4, capability protocol, migration plan).

## Goal

Lift the agent loop out of the TUI process and into a long-running daemon (`borgd`). TUI becomes a gRPC client. Internal transport is gRPC over HTTP/2 on a Unix domain socket (loopback TCP fallback). This is the structural foundation everything else in v1 sits on.

Today the agent runs in-process inside `crates/cli` (REPL + TUI). After this task the agent only runs inside `borgd`; `crates/cli` shells out nothing — it speaks gRPC.

## Scope

### New: `crates/borgd`

- Binary `borgd` with `tokio` runtime.
- Owns: agent loop (lifted from `crates/core/src/agent.rs`), session state, hooks registry, scheduled tasks, heartbeat, gateway adapters, DB handle (one writer).
- Listens on `$BORG_HOME/borgd.sock` (UDS) by default. `--listen 127.0.0.1:PORT` opt-in for loopback.
- Single-instance lock via `flock` on `$BORG_HOME/borgd.pid`.
- Graceful shutdown on SIGTERM (drain in-flight turns, flush DB).

### gRPC schema (`crates/borg-proto`)

Define `.proto` files; codegen Rust (tonic) into `borg-proto`. Codegen for TS/Swift/Kotlin is out of scope for this task — just keep the schema clean enough that downstream codegen will work.

Minimum services:

- `Session`: `Open`, `Send`, `Stream` (server-stream of agent events: tokens, tool calls, system cells), `Cancel`, `Close`.
- `Status`: `Vitals`, `Posture`, `Budget`, `Heartbeat` (used by `/status`).
- `Capability`: bidi-stream. Client advertises capabilities (`file.pick`, `terminal.exec`, `clipboard.read`, `notify`); daemon issues capability requests; client responds. Defines the protocol surface — TUI will implement only the capabilities it has (probably `terminal.exec`, `clipboard`, `notify` at minimum).
- `Admin`: `Pair` (stub now; Task 02 wires it), `ListSessions`, `Shutdown`.

Auth: stub the mTLS hook so Task 02 can drop in cert validation without re-plumbing. Daemon should accept a no-auth UDS connection in dev mode behind `--insecure-uds` (off by default after Task 02 lands).

### TUI rewrite (`crates/cli`)

- Delete in-process agent invocation paths. TUI opens a gRPC `Session.Stream`, renders events, sends user input via `Session.Send`.
- Implement client-side capability handlers: `terminal.exec` (the existing `run_shell` flow runs *in the TUI process*, not the daemon, so workstation tools and shell aliases still work), `clipboard.read/write`, `notify`.
- **Mouse text-selection invariants are non-negotiable.** Re-read CLAUDE.md "Mouse Interaction" section. The guard tests in `crates/cli/src/tui/mod.rs` and `app.rs` must still pass.
- `borg` CLI subcommands that currently run the agent (e.g. `borg poke`, scheduled task dispatch) become gRPC calls.

### Capability protocol

Document the negotiated capability set in code (one source of truth — `crates/borg-proto/src/capabilities.rs`). Daemon stores last-advertised capabilities per connected client. If the agent issues a tool call that needs a capability the connected client doesn't have, fall back to daemon-side execution where possible (e.g. `run_shell` runs in the daemon's sandbox), or surface a clear error.

### Migration of `core`

- Move agent loop, hooks dispatch, DB writer, scheduler from `crates/core` invocations in `cli` into `borgd`. `core` stays a library — `borgd` and any tests link it.
- Keep `crates/core` building as a library for the test suite; do not duplicate code into `borgd`.

## Out of scope for this task

- mTLS, PAKE pairing, audit chain (Task 02).
- New ingress adapters (Task 03) — keep existing gateway code working through the daemon, but don't add OpenAI-compat / webhook / MCP yet.
- Provider plugin refactor + budgets (Task 04).
- Posture changes, retention work (Task 05).

## Done when

- `borgd` starts, accepts a TUI client over UDS, runs a turn end-to-end with streaming tokens visible in the TUI.
- All existing integration tests for the agent loop pass against the new daemon-hosted loop.
- Mouse-selection guard tests still pass; arrow-routing tests still pass.
- `borg poke` triggers a heartbeat via gRPC.
- Two TUI clients can connect to one daemon and observe independent sessions.
- No code path in `crates/cli` calls into `borg_core::Agent` directly.
- `cargo fmt --check && cargo clippy -- -D warnings && cargo test` clean.

## Risks to watch

- **Regressing CLAUDE.md invariants.** Run `grep -R "EnableMouseCapture\|MouseEventKind" crates/cli` before and after; should stay zero.
- **DB writer split-brain.** Only `borgd` writes to `borg.db`. TUI must not open the DB in writeable mode. Add a startup check.
- **Capability fallback ambiguity.** Pick one rule and document it: when a tool needs a capability the client lacks, daemon either (a) runs it locally if safe, or (b) returns a `CapabilityUnavailable` error the agent surfaces. Recommend (a) for `run_shell`, (b) for `clipboard`/`file.pick`.

---

## Handoff status (2026-04-26)

**Shipped (compiling, tested, on `main`):**

- Workspace deps + `borg-proto` + `borgd` crates wired (commit `81476b4`).
- `borgd` daemon: flock + UDS listener, Session/Capability/Status/Admin services,
  `SessionHost` per-session broadcast + ring buffer, `PromptRegistry` for
  `RespondToPrompt`, in-process gateway server spawn from `service::`
  (commits `974c628`, `ab992d5`).
- `crates/cli/src/daemon_client/`: `connect_or_spawn`, ergonomic wrappers,
  pb→core `spawn_event_adapter` (commit `c3ba220`).
- REPL one-shot (`borg ask`, `borg <message>`) migrated to DaemonClient
  (commit `c3ba220`).
- Extended Session proto with agent-control RPCs:
  `GetState`, `Compact`, `ClearHistory`, `Undo`, `RewindToNthUserMessage`,
  `AutoSave`, `NewSession`, `LoadSession`, `Steer` (commit `ad28b67`).
- Extended Admin proto with popup/data-access RPCs:
  `ListProjects`/`Create`/`Update`/`Delete`,
  `ListScheduledTasks`/`Create`/`Delete`/`Pause`/`Resume`,
  `ListPairingRequests`/`Approve`/`ListApprovedSenders`/`Revoke`,
  `GetDaemonStatus` (commit `ad28b67`).
- `SessionBackend` trait gained agent-control surface (default `unsupported`).
  `AgentBackend` implements all of it via the existing `Agent` API. Steer is
  routed through an `mpsc::UnboundedSender` held in `AgentBackend`, with the
  `rx` end installed on the agent at construction (commit `ad28b67`).
- `Send` is now multimodal: `SendRequest.images: Vec<InlineImage>`, threaded
  through `SessionHost::start_turn` and `SessionBackend::run_turn`.
  `AgentBackend::run_turn` dispatches text-only to `send_message_with_cancel`
  and text+images to `send_message_raw` with a `Message::user_multimodal(...)`
  built from base64-encoded `ContentPart::ImageBase64` parts (commit `d5bb230`).
- DaemonClient methods: `get_state`, `compact`, `clear_history`, `undo`,
  `rewind`, `auto_save`, `new_agent_session`, `load_agent_session`, `steer`,
  `send_message_with_images`, plus all the new admin CRUD RPCs.

**Not yet started — pick up here:**

The kernel and wire surface are complete. What remains is migrating the
existing in-process call sites to use that surface, then deleting the
in-process paths.

### 1. TUI runtime rewrite — `crates/cli/src/tui/mod.rs`

The TUI still constructs `Arc<Mutex<Agent>>` at `tui/mod.rs:271-334` and
threads it through `run_event_loop`. Every match arm in `run_event_loop`
(lines 703-1190+) does `let mut agent = agent.lock().await; agent.X();`.
Replace with `DaemonClient` calls. Concrete mapping (each new RPC already
exists in `DaemonClient`):

| TUI call site | New code |
|---|---|
| `Agent::new(...)` + hook registration (lines 271-310) | `DaemonClient::connect_or_spawn().await` — daemon already registers the same hook set in `AgentBackend::new` |
| `agent.set_config_watcher(rx)` (276) | delete — daemon owns config |
| `agent.load_session(meta.id)` (321) | `client.open_session(Some(meta.id)).await` (ignore return), then `client.load_agent_session(session_id, &meta.id)` |
| `Arc::new(Mutex::new(agent))` (334) | `Arc::new(Mutex::new(client))` + a `session_id: String` |
| `AppAction::SendMessage` arm (721-765) | spawn task that calls `client.send_message_with_images(session_id, text, images)`, then forwards `tonic::Streaming<AgentEvent>` events into `event_tx` (translate via `spawn_event_adapter`-style logic, but writing to the existing `mpsc::Sender<core::AgentEvent>` instead of returning a new receiver). The steer channel in `App` (line 728-730) becomes `client.steer(session_id, text)` from wherever steers are sent. |
| `AppAction::CompactHistory` (766) | `let r = client.compact(session_id).await?; (r.dropped, r.after_tokens)` |
| `AppAction::ClearHistory` (774) | `client.clear_history(session_id).await` |
| `AppAction::ShowUsage` — `agent.conversation_stats()` (781) | `let s = client.get_state(session_id).await?; (s.message_count, s.token_estimate)` |
| `AppAction::UndoLastTurn` (838) | `client.undo(session_id).await` |
| `AppAction::RewindTo` (849) | `client.rewind(session_id, n as u64).await` |
| `AppAction::UpdateSetting` (860) | `client.set_setting(&key, &value).await`. Drop the `config_notify_tx` plumbing — daemon's config watcher fires for all clients via a future `Stream` RPC; for now just call `set_setting` and accept that App's local `config` cache goes stale until next `get_settings`. |
| `AppAction::ConfigReloaded` (869) | delete — config lives in daemon |
| `AppAction::SaveSession` (876) | `client.auto_save(session_id).await` then `client.get_state(session_id).await` for the title to display |
| `AppAction::NewSession` (885) | `client.new_agent_session(session_id).await` |
| `AppAction::LoadSession` (900) | resolve prefix locally (still OK — read-only `borg_core::session::resolve_session_id`), then `client.load_agent_session(session_id, &resolved).await` |
| `AppAction::Quit` close-browser hint (705) | call `client.get_state(session_id).await` for the resume hint title |

The `App` struct does **not** own an Agent today, so its struct doesn't need
to change. It only needs `config: Config` to become a snapshot fetched at
startup from `client.get_settings()` and rebuilt from the entries (or just
keep `Config::load_from_db()` since the read path is shared with the daemon).

### 2. TUI popups — drop direct `Database::open()`

Each popup loads its own DB connection. Replace with admin RPCs (all already
exist on `DaemonClient`). Files + new call:

- `tui/projects_popup.rs:102` — `db.list_projects()` → `client.list_projects().await`
- `tui/schedule_popup.rs:71` — `db.list_tasks()` → `client.list_scheduled_tasks().await`
- `tui/pairing_popup.rs:68` — `db.list_pairings()` + `list_approved_senders()` → `client.list_pairing_requests(None).await` + `client.list_approved_senders(None).await`
- `tui/status_popup.rs:160,235,256,281,303` — read vitals/evolution/etc. → either add a `Status.GetVitals` consumer (already wired in `borgd/src/grpc/status.rs`) or extend with the existing `client.get_daemon_status().await`
- `tui/settings_popup.rs:69` — `db` reads → `client.get_settings().await`; writes → `client.set_setting()` / `unset_setting()`
- `tui/sessions_popup.rs` — `client.list_sessions().await` + `client.get_session(&id).await`
- `tui/model_popup.rs:51` — settings via `client.set_setting("llm.model", ...)`
- `tui/commands.rs:329,543,597` and `tui/app/mod.rs:242,255,291,740,960,966,970` — case-by-case; most are session/task/setting reads with admin-RPC equivalents.

The plan calls for a compile-time guard test that `crates/cli` no longer
links `rusqlite` or `borg_core::db` — see the test stub
`crates/cli/tests/grpc_client_smoke.rs::tui_crate_does_not_link_rusqlite_or_borg_core_db`
in the original task description.

### 3. Gateway handler — `crates/gateway/src/handler.rs:927`

Currently `Agent::new(config, metrics)` per inbound message. Replace with
`DaemonClient::connect_or_spawn().await` + `client.open_session(None)` +
`client.send_message_with_images(...)` + drain stream → reassemble text.
`in_flight::GLOBAL` (`handler.rs:378,947,999`) becomes
`client.cancel(session_id)`.

The gateway handler is run by `borgd` itself (via `spawn_gateway` in
`crates/cli/src/service/mod.rs` — that spawn currently lives in both `cli`
and `borgd`). When this lands, only the `borgd`-side spawn remains.

### 4. Move heartbeat scheduler + gateway server spawn out of `service/`

`crates/cli/src/service/mod.rs` (2245 LOC) hosts:

- `run_daemon(shutdown)` — the `borg daemon` subcommand entry point, runs
  scheduled-task dispatcher + heartbeat scheduler + gateway server.
- `execute_heartbeat_turn(config, source)` — used by TUI for in-process
  heartbeat firing.
- `send_approval_greeting(config, channel, sender_id)` — used by TUI/pairing
  popup for first-contact greeting after manual approval.
- `HeartbeatScheduler`, `HeartbeatSource`, `HeartbeatEvent`, `HeartbeatResult`
  types referenced from `tui/mod.rs`.

All of this needs to move into `borgd` proper. The TUI then either:
- starts a heartbeat run via `client.poke().await` (already exists), or
- subscribes to a future `Status.HeartbeatStream` RPC that pushes
  heartbeat events from the daemon.

The `send_approval_greeting` flow becomes an Admin RPC, e.g.
`Admin.SendApprovalGreeting(channel, sender_id)`, that the daemon executes
using its own session machinery.

### 5. Delete `service/` + `in_flight.rs` + downstream callers

After (3) and (4) land:

- `crates/cli/src/service/` — delete the directory.
- `crates/cli/src/in_flight.rs` — delete; replaced by `DaemonClient::cancel`
  routed through `borgd::session::SessionHost::cancel_turn`.
- `crates/cli/src/main.rs:353,367-368,379,502` — drop `service::*` calls.
  `borg daemon` becomes `exec borgd` (or removed entirely if `borgd` is
  installed as a separate binary).
- `crates/cli/src/commands/misc.rs:37,38,300,303` — `service::*` →
  `DaemonClient::shutdown()` for stop, drop install/uninstall (those are
  launchd/systemd tasks for `borgd`, not the CLI).
- `crates/cli/src/commands/pairing.rs:102` — `service::send_approval_greeting`
  → `client.send_approval_greeting(...)` (the new Admin RPC from step 4).
- `crates/cli/src/plugins.rs:293` — `service::restart_service` → either drop
  or re-wire to `DaemonClient::shutdown()` and let the launcher restart.

### 6. Compile-time guard tests

Add per the original plan:

- `crates/cli/tests/grpc_client_smoke.rs` — `cargo metadata`-based test
  asserting `crates/cli` no longer links `rusqlite` or
  `borg_core::db::*`.
- `crates/borgd/tests/single_writer_lock.rs` — verify second `borgd`
  startup fails fast when the first holds the flock; verify stale-PID
  recovery after SIGKILL of the first.
- `crates/borgd/tests/capability_routing.rs` — terminal_exec fallback to
  daemon when no client; clipboard.read returns capability-unavailable when
  no client.

### Verification (when 1–6 land)

The plan's `Done when` checklist still applies. Specifically:
`grep -R "EnableMouseCapture\|MouseEventKind" crates/cli` must stay zero,
and the 11 mouse guard tests in `crates/cli/src/tui/mod.rs:1929-2047` and
`crates/cli/src/tui/app.rs` must still pass against whatever new file
layout the rewrite produces (their `include_str!` paths will need
updating if files move).
