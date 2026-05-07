# Roadmap

Tracked follow-ups from shipped work. Each entry names the source plan or
commit that deferred the work so context isn't lost.

## Pre-launch hardening

Items uncovered by the 2026-04 pre-launch audit. Cleared items move out;
unfinished items stay here so a future contributor can see what's left.

- **Shell-policy bypass surface.** `crates/cortex/src/policy.rs` now
  recovers stacked backslash escapes, backticks (with recursive body
  deobfuscation), and literal `$(cmd)` substitutions before the deny
  recheck. Substitutions whose value is computed at runtime
  (`$(echo rm) -rf /`) still slip past, and the deobfuscator is
  quote-unaware, so a deny rule keying on later tokens could over-match
  text inside `'...'`. The policy is documented best-effort and the
  sandbox is the real boundary; a future `shell-words`-based parser
  would close both gaps.
- **TOCTOU on blocked-path checks.** `is_blocked_path()` resolves full
  symlink chains via `canonicalize()` and a regression test pins the
  behavior. The window between check and open is acknowledged in the
  threat model. A defense-in-depth pass using `O_NOFOLLOW` on Linux
  opens or `/proc/self/fd` re-canonicalization would close it; deferred
  until there's a credible exploit path.
- **CI coverage gate.** CLAUDE.md targets 80%+ but `.github/workflows/`
  has no failing threshold. Add `cargo llvm-cov --fail-under-lines 80`
  once the daemon session executor and scheduler grow real coverage,
  or soften the doc claim.
- **Nightly `--include-ignored` job.** `crates/cortex/src/browser/mod.rs`
  and the `borgd_launcher` integration tests are `#[ignore]`'d behind
  Chrome and daemon-spawn requirements. A nightly CI lane with those
  prerequisites would catch first-run regressions before users do.

## Self-healing — Tier 2 / Tier 3

- **Provider failover / degraded mode.** Deferred. Add structured
  provider error classification plus bounded failover to fallback
  providers / Claude CLI / Ollama so primary-provider auth, rate-limit,
  or overload incidents degrade gracefully instead of dead-ending the
  agent loop.
- **Browser recovery loops.** Deferred. Attach automatic restart /
  reconnect behavior to recoverable browser-health failures so stale CDP
  sessions, crashed tabs, and disconnected browser processes repair
  themselves without waiting for a manual restart.
- **Plugin / skill quarantine.** Deferred. Integrity and tamper checks
  currently surface warnings; a future pass should mark compromised
  plugins or user skills untrusted, stop loading them automatically, and
  require explicit re-trust with a visible audit trail.
- **Backup verification.** Deferred. `borg backup create` produces
  manual tarballs but the daemon does not periodically verify that older
  ones still restore cleanly. Add archive verification, manifest/hash
  checks, and periodic test-restore drills into a temp directory so
  "backup succeeded" also means "restore is likely to work."
- **Sleep-inhibitor wire-up.** Deferred. A multi-platform RAII
  `SleepInhibitor` (macOS `caffeinate -i -w <pid>`, Linux
  `systemd-inhibit --what=sleep ... sleep infinity`, no-op elsewhere)
  shipped in 2026-04 but was never instantiated by any caller and was
  removed by the 2026-05 dead-code sweep. Restore it and hold a guard
  for the lifetime of an active session and/or running scheduled task in
  `borgd::session` / `borgd::task_runner` so a laptop borg doesn't get
  suspended mid-tool-call. Restoration is mechanical — pull the file
  from history (last present at `be2ae863`), re-add `pub mod
  sleep_inhibitor;` in `crates/cortex/src/lib.rs`, then add the call
  sites the original feature was missing.
- **CVE advisory integration.** Deferred. Pulling rustsec / OSV advisories
  on a cadence would mean an outbound git/HTTP fetch from a background
  task, which is unwelcome on airgapped or locked-down deployments and
  adds non-trivial binary weight for a feature many installs won't use.
  Revisit if user demand materializes — a `cargo audit` wrapper around
  `Cargo.lock` in the release binary is one safer path.

## Telegram voice follow-through

- **Roll out shared voice I/O policy to the rest of the gateway.**
  Telegram now has shared voice-policy resolution, per-chat `/voice`
  modes, inbound audio/voice transcription, and outbound voice replies
  wired through reusable gateway helpers. Extend the same pattern to
  Discord, Slack, Twilio/WhatsApp, Signal, Teams, Google Chat, and
  iMessage in a future session instead of re-implementing per-platform
  toggles ad hoc.

## Headless KEK — passphrase lifecycle

The headless-VPS path (`security.kek_backend = file`) ships in v0.1.x as
a passphrase-encrypted file at `~/.borg/secrets/kek.enc` with the
passphrase loaded from `~/.borg/secrets/kek.env` via systemd
EnvironmentFile. Several lifecycle gaps remain — none block deployment;
all warrant follow-up work.

- **Passphrase rotation.** Today the passphrase is generated once at
  install time and never rotates. Add a `borg keychain rotate` command
  that re-encrypts the KEK blob under a fresh passphrase, atomically
  swaps `kek.env`, and triggers a daemon reload. Without rotation,
  long-lived deployments accrete the same secret on backup tapes
  indefinitely.
- **Recovery.** Lost passphrase = re-bootstrap CA + re-pair every
  device. Document a `borg keychain rebootstrap` command that wipes
  `kek.enc` + paired-device rows + re-mints the CA in one shot, with a
  clear "this revokes all paired devices" warning and a `--force` gate.
- **Promotion `file → os_keychain`.** When the operator runs
  `borg service install --secure` on an install that's been on the file
  backend, decrypt `kek.enc`, write the entries into the OS keychain,
  flip `security.kek_backend = os_keychain`, and remove the file. The
  posture transition then fires the existing `secure_headless_setup`
  milestone (+100 XP). Today the user has to re-bootstrap manually.
- **Demotion `os_keychain → file`.** Symmetric: dump from keychain into
  a freshly-encrypted file, switch the setting, restart. Useful for
  exporting an install for an air-gapped environment.
- **Headless onboarding TUI screen.** Today
  `crates/interface/src/borgd_launcher.rs::print_headless_setup_hint`
  prints a stderr block at the end of `borg init`. A proper two-button
  screen in `onboarding_tui.rs` ("Install secure (recommended)" /
  "Continue with file backend") would let the user opt into apt
  installs interactively rather than running `borg service install
  --secure` as a separate step.
- **`borg doctor` surface.** Surface the active backend + posture
  multiplier in `borg doctor` output so a user investigating low XP
  rate has an obvious explanation pointer rather than digging through
  logs.

## Reference-survey backlog

Items pulled from a 2026-04 survey of two adjacent agent codebases.
Source tags: `[O]` and `[H]` indicate which reference project the
pattern came from; `[O+H]` = both. Each item is independent — work
top-down by priority, not by section order.

### Resilience & self-healing

- **`borg doctor` repair modes.** `[O]` Today `borg doctor` is mostly
  read-only. Add `--repair` / `--fix` / `--force` that actually mutate
  with backups: legacy config-shape migrations, stale session lock
  removal, orphan transcript archival, corrupted auth-profile repair.
  Pairs with the existing daily self-heal task — doctor is the
  on-demand surface, self-heal is the cron surface.
- **Session/transcript JSON repair on load.** `[O]` Detect corrupted
  `messages` rows or orphaned attachment blobs at session-open time
  and repair (truncate to last clean turn, archive bad blobs) instead
  of crashing the agent loop on the next launch.
- **Stale session lock detection.** `[O]` If a daemon crashes
  mid-session, the next launch should detect and clear the orphaned
  lock rather than dead-ending. Folds into `borg doctor`.
- **Error classifier for LLM API failures.** `[H]` Centralized
  taxonomy (`auth` / `billing` / `rate_limit` / `overloaded` /
  `timeout` / `context_length`) returning a structured `FailoverReason`
  that drives recovery strategy. Replaces the scattered string-matching
  spread across provider modules today.
- **Credential pool with exhaustion tracking.** `[H]` Multiple
  credentials per provider with cooldown TTL, auto-rotation on 402/429,
  and selectable strategy (`round_robin` / `least_used` / `fill_first`).
  Today a single exhausted key dead-ends the provider.
- **Auxiliary-LLM failover chain.** `[H]` Side tasks (context
  compression, summarization, embedding fallback) resolve through a
  separate provider chain so a primary outage doesn't kill background
  work like nightly memory consolidation.
- **`run.attempt` metadata in logs.** `[O]` Tag each line in
  `~/.borg/logs/{date}.jsonl` with the retry attempt number so
  duplicated tool calls or agent turns are correlatable when grepping.

### Streaming & channel delivery

- **Block streaming with coalescing.** `[O]` Stream completed semantic
  blocks (paragraph → newline → sentence) to channels rather than raw
  token deltas. Honors code-fence safety + min/max char windows.
  Reduces Telegram/Slack edit thrash (currently every token is an
  `editMessage` call).
- **Preview-edit streaming per channel.** `[O]` Telegram/Slack/Discord
  show a single edited preview message while generating, then finalize.
  Falls back to one-shot send if the edit API fails mid-stream.
- **Final-delivery integrity fallback.** `[O]` If streaming breaks
  mid-reply, the user MUST still see the full final reply via a
  one-shot send. Today a streaming failure can drop the tail of a
  message silently.
- **Per-channel health states.** `[O]` Track `online` / `degraded` /
  `offline` per gateway binding and surface in `borg status`. Today we
  have no notion of a sick channel — a Telegram outage is invisible
  until the user complains.

### Context & memory

- **Mid-turn context compression with "Active Task" framing.** `[H]`
  When approaching context limit, summarize middle turns via auxiliary
  LLM while preserving head + tail. Explicit "Active Task" handoff
  framing prevents the model from treating the summary as new
  instructions to re-execute.
- **Trajectory compressor for long sessions.** `[H]` Offline tool that
  compresses old conversation logs into structured summaries, freeing
  DB space and improving retrieval recall. Distinct from in-turn
  compression — runs as a maintenance pass.
- **Session insights (`/insights`).** `[H]` Per-session token spend,
  cost estimate, tool-call distribution, model usage, activity trends
  by channel. Slash command + TUI panel.
- **Prompt-cache hit/miss observability.** `[O]` Structured logging of
  cache creation/hit/miss counts per turn. We use Anthropic prompt
  caching but currently can't measure whether it's actually working.

## Cost emission: additional providers

- **Groq.** Emit `cost_usd_present = false` until provider pricing is wired into the static table path.
- **Together.** Emit `cost_usd_present = false` until provider pricing is wired into the static table path.
- **Fireworks.** Emit `cost_usd_present = false` until provider pricing is wired into the static table path.
- **Bedrock.** Emit `cost_usd_present = false` until provider pricing is wired into the static table path.
- **Vertex.** Emit `cost_usd_present = false` until provider pricing is wired into the static table path.
- **Local models.** Emit `cost_usd_present = false` until a local-pricing policy exists.

### Tools & sub-agents

- **`delegate` subagent tool.** `[H]` Spawn a child agent with
  restricted toolset, fresh context, and depth/concurrency limits
  (`MAX_DEPTH=2`). Useful for "go research X" or "draft Y in parallel"
  without polluting main context. Distinct from workflows (which are
  sequential).
- **Server-initiated MCP sampling.** `[H]` If/when we add MCP client
  support, allow MCP servers to request completions back through Borg
  (full MCP spec, not just tool-calling).
- **Expanded dangerous-command regex set.** `[H]` 50+ patterns:
  `chmod 777`, fork bombs, `curl|sh`, `tee` overwrite of credential
  files, `bash -c` payload introspection, recursive deletes outside
  cwd. Today the sandbox blocks a small handful of catastrophic
  commands; this is the next layer.
- **Code-execution sandbox tool.** `[H]` Run user-provided Python (or
  Rust via `cargo script`) in a subprocess with a restricted stub
  library exposing only safe tools (web_search, read/write_file,
  search_files, patch, terminal). Resource caps on stdout/stderr/
  timeout/tool-calls. Useful for ad-hoc data crunching.

### Plugins & extensibility

- **Plugin manifest + onboard hooks.** `[O]` `plugin.json` declaring
  deps, capabilities, and an onboard script that runs at install time
  to configure auth/defaults. Load-time validation rejects invalid
  plugins instead of crashing the daemon.
- **Plugin SDK with narrow seams.** `[O]` Documented entrypoints +
  lazy `*.runtime` subpaths so plugins load metadata cheaply but defer
  heavy imports. Keeps daemon startup fast as the plugin marketplace
  grows.
- **Doctor-driven plugin dep auto-install.** `[O]` `borg doctor
  --repair` installs missing bundled-plugin runtime deps (respecting
  explicit disablement). External plugins still manual.

### Onboarding & migration

- **Modular setup wizard.** `[H]` Independently re-runnable sections:
  model picker → terminal/sandbox backend → channels → tools →
  credentials. Each section idempotent, invokable alone via
  `borg setup <section>`. Today onboarding is one linear flow.
- **Cross-tool migration importer.** `[H]` `borg import claude-code` /
  `borg import codex` that pulls `AGENTS.md` / `CLAUDE.md` / hooks /
  approved-command lists / persona files into Borg's memory + config.
  Dry-run + overwrite presets. Lowers switching cost from other agents.

### Security & sandboxing

- **Docker / SSH sandbox backends.** `[O]` In addition to Seatbelt
  and bwrap, allow agent-issued shell to run inside a Docker container
  or over SSH to a remote VM. Makes Linux desktop installs (where
  bwrap may be unavailable) viable and enables true "run on a VPS"
  isolation for sensitive workloads.
- **noVNC observer for browser tool.** `[O]` When the `browser` tool
  is active, expose a short-lived noVNC URL (token in URL fragment,
  not query) so the user can watch what the agent is doing in real
  time.
- **`INCIDENT_RESPONSE.md` + severity tiering.** `[O]` Public incident-
  response runbook with critical/high/medium/low tiers, coordinated
  disclosure rules, CVE issuance criteria. `SECURITY.md` exists; this
  is the operational complement.
- **Env-var allowlist for shell exec.** `[H]` Whitelist of ~200
  known-safe env vars passed into agent-spawned subprocesses;
  everything else stripped. Prevents accidental leak of
  `OPENROUTER_API_KEY` / `ANTHROPIC_API_KEY` etc. into shell tool
  output that gets logged or echoed back.

### Observability

- **Optional OpenTelemetry export.** `[O]` `[telemetry] otel_endpoint`
  setting; emit traces/metrics/logs for model calls, tool calls,
  webhook processing. Off by default. Targets power users running
  Borg as a long-lived service.
- **`borg health --json`.** `[O]` Machine-readable component status
  (DB / KEK / channels / sandbox / LLM provider). Distinct from
  `borg doctor` (fix-oriented) and `borg status` (vitals-only) —
  this is for monitoring integration.

### Scheduling

- **File-lock on cron tick.** `[H]` Prevent overlapping scheduler
  ticks if the daemon hiccups and two ticks land simultaneously.
  Today nothing prevents double-fire of a 5-minute task.
- **Heartbeat `lightContext` + `isolatedSession` modes.** `[O]`
  Heartbeat options: load only `HEARTBEAT.md` (skip full memory) or
  run in a fresh session with no history. Useful for "just check my
  calendar" cadences that shouldn't pollute the main thread.
- **Heartbeat active-hours per binding.** `[O]` Per-channel active
  hours, separate from the global 00:00–06:00 quiet window. Lets a
  user receive nightly digests on email but not on Telegram.

### IDE integration

- **ACP adapter for VS Code / Zed / JetBrains.** `[O+H]` Run `borgd`
  as an ACP server so Borg can be the in-IDE coding agent. Reuses
  existing tools + memory + skills + sandboxing.

### Batch / RL infrastructure

- **Trajectory export for training.** `[H]` `borg export trajectories`
  writes from/value pairs in HuggingFace-dataset schema, with
  checkpointing for resume. Useful if we ever fine-tune small local
  models on Borg's tool-call patterns.
- **RL environment harness.** `[H]` Atropos-compatible gym
  environments (web research, terminal test, SWE tasks) for sampling
  and evaluation. Listed for completeness — likely defer until there's
  a concrete training initiative.

### Potential features (not committed)

Ideas worth keeping an eye on. **Not** scheduled. Each trades real
complexity for a benefit Borg doesn't currently offer — revisit if user
demand materializes.

- **Tool receipts.** Cryptographically chained signed receipts on every
  tool call (sibling to the HMAC vitals chain, but tool-call-scoped
  rather than event-sourced). Source of truth for "what did the agent
  do yesterday." Better audit primitive than what Borg has today;
  matters for compliance use cases. Cost: a new chain to maintain,
  schema decisions, a CLI surface to query receipts.
- **Hardware / IoT peripherals.** GPIO / I2C / SPI / USB on Raspberry
  Pi, STM32, Arduino, ESP32 via a `Peripheral` trait. Out of scope for
  current Borg users but would open homelab/SBC deployments. Cost:
  cross-compilation matrix, hardware test rigs, embedded-Rust
  expertise.
- **WASM plugin system.** Capability-sandboxed third-party extensions
  (compile to WASM in any language, host grants narrow capabilities,
  endpoint allowlist + leak scanner at the host boundary). Today Borg
  is compiled-in only; this is the path to a real plugin ecosystem
  without the security cost of native loading. Cost: WASM runtime in
  the binary (`wasmtime` ≈ several MB), capability-grant API design,
  manifest signing/verification, confused-deputy hardening on every
  host function.

### Tests & quality

- **Live-LLM test lane.** `[O]` `BORG_LIVE_TEST=1 cargo test --features
  live` for smoke tests that hit real APIs. Gated, off by default.
  Catches provider regressions (auth flow changes, response shape
  drift) that mocked tests can't see.
- **Hot-path startup-time perf tests.** `[O]` CI test that fails if
  cold-start daemon time or system-prompt-assembly time regresses
  past a threshold. Both are user-visible and tend to drift up.
- **Architecture/boundary tests.** `[O]` Static checks asserting
  `interface` doesn't reach into `cortex`'s private modules, gateway
  crates don't depend on TUI, etc. Could be `cargo-deny` rules or a
  custom lint.
