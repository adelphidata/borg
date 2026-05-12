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
- **`run.attempt` metadata in logs.** `[O]` Tag each line in
  `~/.borg/logs/{date}.jsonl` with the retry attempt number so
  duplicated tool calls or agent turns are correlatable when grepping.

### Provider abstraction

- **Transport trait for provider protocols.** `[H]` Deferred. Today each
  LLM provider has hand-rolled retry, streaming, and caching paths. A
  `Transport` trait covering the major shapes (Anthropic / ChatCompletions
  / Responses / Bedrock / Gemini) would normalize those paths so adding a
  new provider is roughly a ten-line job instead of a full module. Slots
  in under the credential-pool work already in flight.
- **Live model discovery manifest.** `[H]` Deferred. Fetch a remote model
  catalog (price, context window, capabilities) at startup instead of
  carrying compiled-in lists. Operators see new Claude / GPT / Gemini
  models in the picker without waiting for a Borg release. Pairs with the
  cost-emission roadmap items below.
- **Per-turn auxiliary model override.** `[H]` Deferred. Pick a different
  model for a single tool call or turn — vision-capable for one
  screenshot analysis, cheap default for the rest. Today Borg switches
  the whole gateway binding; per-turn override is finer-grained. CLI flag
  plus a TUI affordance.
- **Proactive rate-limit tracker.** `[H]` Deferred. Track observed
  token/request rates per provider and rotate keys _before_ the provider
  returns 429 instead of reacting to it. Extends the credential pool
  (currently reactive — waits for the 429) with proactive avoidance.
  Reduces mean latency under sustained load.
- **Additional providers (prioritized).** `[O+H]` Deferred. Enterprise +
  frontier gaps surfaced by the 2026-05 reference survey. Self-hosted
  endpoints (LM Studio, vLLM, llama.cpp) already work via the custom
  OpenAI-compatible credential type — no dedicated provider needed.
  Order is priority, not alphabetical:
  1. **AWS Bedrock.** SigV4-signed Anthropic/Llama access; the
     enterprise gateway for shops that can't ship API keys to vendors.
     Needed for any serious AWS-shop adoption.
  2. **Azure OpenAI.** Deployment-name-scoped OpenAI on Microsoft
     tenancy with AAD auth. Same enterprise rationale as Bedrock;
     covers the Microsoft half of the market.
  3. **Google Vertex AI.** Gemini + Anthropic via GCP service-account
     auth. Rounds out the big-three cloud gateways and unlocks Gemini
     for orgs that ban consumer Google AI Studio keys.
  4. **xAI (Grok).** Frontier model with a long context window and an
     OpenAI-compatible surface; cheap to add and currently a real
     capability gap vs. Anthropic/OpenAI/Gemini.
  5. **Cerebras.** Wafer-scale inference, fastest tokens/sec for
     Llama/Qwen; ideal `llm.aux` target where latency dominates quality.
  6. **Together AI / Fireworks.** Broad OSS-model hosting
     (Llama/Qwen/DeepSeek). Either covers the "I want OSS without
     self-hosting" lane; pick one.

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
- **Channel mirror/relay.** `[H]` Deferred. Rule-based forwarding between
  channels (Telegram inbound → Slack notification, Discord activity →
  email digest). Non-trivial config surface; useful for multi-platform
  users running a single Borg.
- **Per-channel auto-restart supervisor.** `[H]` Deferred. Today a wedged
  Telegram poll loop requires a full daemon restart. A per-channel
  supervisor with state preservation across restart folds into the
  `spawn_supervised` invariant — each channel gets its own watchdog and
  backoff.
- **Webhook direct-delivery mode.** `[H]` Deferred. Allow webhook routes
  to skip the LLM and deliver formatted output straight to a channel —
  for low-value high-frequency payloads (uptime pings, log alerts, CI
  notifications). Cost/latency win for monitoring-style use; adds a
  formatter config surface.
- **WhatsApp Business native channel.** `[H]` Deferred. Today Borg
  reaches WhatsApp via the Twilio bridge. A direct WhatsApp Business
  integration would skip Twilio's per-message markup and unlock
  first-class threading, reactions, and media.

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
- **Prompt-cache hit/miss observability.** `[O]` Structured logging of
  cache creation/hit/miss counts per turn. We use Anthropic prompt
  caching but currently can't measure whether it's actually working.
- **Tool result spill-to-disk with handles.** `[H]` Deferred. Large tool
  outputs (e.g. a `run_shell` returning 200 KB of logs) get persisted to
  a temp file and replaced in context with a handle reference; the agent
  re-reads on demand via `read_file`. Prevents context bloat from a
  single chatty tool call. Pairs with the deferred mid-turn context
  compression.

### Agent loop ergonomics

- **Mid-run steering.** `[H]` Deferred. Operator sends a directive
  between turns without restarting the session — "skip the auth module,
  try the parser instead." Injected as a system-level nudge for the next
  turn, not as a user message, so the assistant's reply chain stays
  clean. Removes the restart cycle for long-running autonomous tasks.
- **Todo tool / in-session task list.** `[H]` Deferred. Agent-managed
  task list persisted across turns and visible to the operator. Borg has
  `scheduled_tasks` (cron-style) but no in-session lightweight TODO
  surface. Possible Tool Conservation play: add as a `schedule.todo`
  action rather than a new tool.
- **Session search tool.** `[H]` Deferred. Full-text + semantic search
  across prior conversations as an agent-callable tool, distinct from
  `memory_search` which only hits consolidated long-term entries. "What
  did I do last Tuesday?" works today only if the relevant turn was
  consolidated — live session search is a separate retrieval pass.
- **Session title auto-generation.** `[H]` Deferred. Cheap aux-LLM call
  after the first user turn that names the session for transcript-pager
  navigation. Small UX win; uses the existing `llm.aux` background
  client.
- **Mixture-of-agents tool.** `[H]` Deferred. Query multiple models in
  parallel and synthesize a single answer. High cost; reserved for
  high-stakes single-turn use cases. Niche — defer until explicit
  demand.
- **Structured clarify tool.** `[H]` Deferred. Multi-choice clarification
  with structured options the operator picks from, distinct from
  `request_user_input` (free-form). Marginal value; likely covered by
  extending `request_user_input` with an `options` parameter rather than
  adding a new tool.

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

### MCP & external integrations

- **MCP server mode (`borgd serve-mcp`).** `[H]` Deferred. Expose
  sessions, messages, events, and permissions as MCP tools so external
  IDEs (Claude Code, Cursor, Codex) can read and write Borg
  conversations. Same surface as the existing ACP transport but
  IDE-facing. Useful for "view my Borg state from my editor" workflows.
- **MCP OAuth client.** `[H]` Deferred. Borg as MCP **client** with an
  OAuth flow for servers that require it. Most enterprise MCP servers
  do — without OAuth, a meaningful slice of the existing MCP ecosystem
  is unreachable. Tokens stored in the keychain alongside paired-device
  certs.
- **OSV malware check for third-party subprocesses.** `[H]` Deferred.
  Pre-flight query against the OSV database before spawning any `npx` /
  `uvx` / pip-installed MCP subprocess. Refuse on known-bad; cache
  results with TTL. Concrete supply-chain hardening that aligns with
  the security-first posture; light footprint (lookup table plus a
  cached HTTP fetch).

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

### Skill ecosystem

- **Autonomous skill curator.** `[H]` Deferred. Background task grades,
  dedupes, and consolidates the skill library on a cadence, writing a
  ranked report under `~/.borg/logs/curator/` and flagging
  low-performing or unsafe skills for removal. Distinct from the
  existing `skill_audit` tamper-only detection — this is quality and
  decay-rate auditing. Folds into the daily self-heal sweep.
- **Agent-callable skill install/sync.** `[H]` Deferred. Today skills
  are baked into the binary or hand-placed at `~/.borg/skills/`. A new
  `skill_manager` action would let the agent install or sync skills
  from a curated hub at runtime, with per-skill usage tracking so the
  curator can rank them. Pairs with Tool Conservation — adds actions to
  an existing tool rather than a new tool.
- **Skill provenance + signed manifests.** `[H]` Deferred. Skills carry
  signed provenance (origin, author, content hash chain) and the loader
  verifies signatures before invoking. Extends `skill_audit`
  (tamper-only) toward a full supply-chain story. Useful once skills
  can be agent-installed from a hub.

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
- **Browser anti-detection + supervisor.** `[H]` Drop-in
  browser provider with anti-bot stealth headers, dialog auto-handling,
  and a supervisor that restarts crashed sessions. Borg's `browser`
  exists but anti-detection posture is unverified, and the deferred
  Tier 2 browser-recovery work should adopt the same supervisor.

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

- **Heartbeat `lightContext` + `isolatedSession` modes.** `[O]`
  Heartbeat options: load only `HEARTBEAT.md` (skip full memory) or
  run in a fresh session with no history. Useful for "just check my
  calendar" cadences that shouldn't pollute the main thread.
- **Heartbeat active-hours per binding.** `[O]` Per-channel active
  hours, separate from the global 00:00–06:00 quiet window. Lets a
  user receive nightly digests on email but not on Telegram.

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
- **Web dashboard.** `[H]` Optional remote-monitoring web UI for
  headless/VPS deployments — React SPA served by `borgd` over the
  existing mTLS-authed TCP listener. Conflicts with the TUI-first
  design and adds a frontend dependency to the single-binary story.
  Revisit if headless deployments materialize as a major user segment.
- **Knowledge-graph memory backend.** `[H]` Heavyweight alternative to
  the current hybrid vector+BM25 memory retrieval — entity resolution,
  multi-strategy retrieval, reflect-style synthesis. Adds a
  daemon/process plus an entity-resolution service. Revisit only if
  measured hybrid-retrieval recall underperforms.
- **Home Assistant tool.** `[H]` Smart-home entity control
  (`list_entities`, `get_state`, `call_service`) gated on a
  `HASS_TOKEN`. Niche; revisit if explicit personal-use demand emerges.
- **Offline local TTS bundled in-binary.** `[H]` Neural-TTS model
  embedded so `text_to_speech` works without external API calls.
  Trades single-binary size budget for offline capability. Skip unless
  airgapped deployments push for it.
- **Image-gen provider routing.** `[H]` Multi-provider router for
  `generate_image` (DALL-E vs SDXL vs Imagen based on cost or
  capability). Marginal benefit; skip unless explicit cost/quality
  complaints surface.
- **Vision tool for image analysis.** `[H]` Standalone image-analysis
  tool (screenshots, photos). Most providers now accept images directly
  in the message stream, making a separate tool partly redundant.
  Defer until concrete demand emerges.

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
