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

- **Plugin / skill quarantine.** Deferred. Integrity and tamper checks
  currently surface warnings; a future pass should mark compromised
  plugins or user skills untrusted, stop loading them automatically, and
  require explicit re-trust with a visible audit trail.
- **Backup command + verification.** `[I]` Deferred. First-class local
  backup archives (`borg backup` writing a `~/.borg/` tarball with
  manifest), plus periodic verification that older archives still
  restore cleanly: archive verification, manifest/hash checks, and
  test-restore drills into a temp directory so "backup succeeded" also
  means "restore is likely to work." Today `borg backup create` produces
  manual tarballs without verification or first-class CLI ergonomics.
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

## APNs push pipeline

The DB schema (`apns_device_tokens`, migration v3), gRPC surface
(`Apns.IssueChallenge` / `RegisterDevice` / `UnregisterDevice`), push
trigger, and 410-invalidation paths shipped in 2026-05 as part of the
networking-bundle PR. The App Attest verifier is the one remaining gap.

- **App Attest verifier.** Today
  `crates/borgd/src/apns/attest.rs::verify_attestation` returns
  `AttestError::NotImplemented`, so `Apns.RegisterDevice` rejects every
  request. Real verifier needs: CBOR decode of the attestation object,
  x5c chain validation against Apple's App Attest root CA (PEM compiled
  in), nonce extraction from credCert extension OID
  `1.2.840.113635.100.8.2`, `sha256(authData || sha256(challenge))`
  equality check, RP ID (`sha256(bundle_id)`) match against
  `authData[0..32]`, UP-bit + counter==0 assertion, credentialId equality
  against `key_id`, and P-256 SPKI extraction. Test harness expectations
  already in place via the cfg(test) root-CA seam — the verifier just
  needs to land. The `a2` push client is also stubbed; production wiring
  is a separate paragraph of the same PR.

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

Items pulled from a 2026-04 survey of two adjacent agent codebases,
plus a 2026-05 survey of two more. Source tags: `[O]`, `[H]`, `[Z]`,
and `[I]` indicate which reference project the pattern came from;
`[O+H]` = both of the first two. Each item is independent — work
top-down by priority, not by section order.

### Resilience & self-healing

- **`borg doctor` — expanded checks + repair modes.** `[O][I]` Today
  `borg doctor` is mostly read-only and covers a narrow slice. Two
  axes of growth: (1) broader checks (~16 subsystems — settings
  validity, LLM credential reachability, DB integrity, embeddings
  cache, scheduled tasks, gateway bindings, MCP servers, skills load,
  secrets/keychain, service install state, Docker daemon presence,
  tunnel binaries); (2) `--repair` / `--fix` / `--force` that actually
  mutate with backups — legacy config-shape migrations, stale session
  lock removal (clear orphaned locks when a daemon crashed
  mid-session), orphan transcript archival, corrupted auth-profile
  repair. Pairs with the daily self-heal task — doctor is the
  on-demand surface, self-heal is the cron surface.
- **Session/transcript JSON repair on load.** `[O]` Detect corrupted
  `messages` rows or orphaned attachment blobs at session-open time
  and repair (truncate to last clean turn, archive bad blobs) instead
  of crashing the agent loop on the next launch.
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
- **Live model discovery manifest.** `[H][I]` Deferred.
  Manifest-backed model catalog fetched at startup (price, context
  window, capabilities, aliases, suppressions) instead of carrying
  compiled-in lists, with a cold installed-index fast path and
  pricing-catalog stale-cache fallback for offline installs. Operators
  see new Claude / GPT / Gemini models in the picker without waiting
  for a Borg release. Pairs with the cost-emission roadmap items below.
- **Per-turn auxiliary model override.** `[H]` Deferred. Pick a different
  model for a single tool call or turn — vision-capable for one
  screenshot analysis, cheap default for the rest. Today Borg switches
  the whole gateway binding; per-turn override is finer-grained. CLI flag
  plus a TUI affordance.
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
    4. **Cerebras.** Wafer-scale inference, fastest tokens/sec for
       Llama/Qwen; ideal `llm.aux` target where latency dominates quality.
    5. **Together AI / Fireworks.** Broad OSS-model hosting
       (Llama/Qwen/DeepSeek). Either covers the "I want OSS without
       self-hosting" lane; pick one.

- **CLI-delegate providers: `codex_cli`, `gemini_cli`, `opencode_cli`.**
  `[Z]` Deferred. New providers that piggyback on the user's locally-
  installed OpenAI / Gemini / OpenCode coding CLIs the way the existing
  `Provider::ClaudeCode` piggybacks on the Claude Code CLI — auth,
  rate limits, and quota come from the CLI's existing session rather
  than Borg-managed API keys. Lowers per-provider onboarding to zero
  for users who already have those tools installed, and gives Borg a
  cheap path to whatever models those vendors gate behind subscription
  rather than API access. Provider-shaped (one credential type each),
  not tools.

    Shipped (no further work):
    - **xAI (Grok)** — `Provider::XAi` direct integration with the OpenAI-
      compatible chat-completions surface, Bearer auth via `XAI_API_KEY`,
      Grok 4 / Grok 4 Fast (reasoning + non-reasoning) in the model
      picker, aux default `grok-4-fast-non-reasoning`, registry entries
      and onboarding wiring. Pricing intentionally deferred — see "Cost
      emission" below.

### Streaming & channel delivery

- **Block + tool-level streaming with coalescing.** `[O][I]` Stream
  completed semantic blocks (paragraph → newline → sentence) and
  tool-result chunks incrementally to channels rather than raw token
  deltas. Honors code-fence safety + min/max char windows. Reduces
  Telegram/Slack edit thrash (currently every token is an
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
- **WhatsApp native channel.** `[H][Z]` Deferred. Today Borg reaches
  WhatsApp via the Twilio bridge, which adds per-message markup and
  loses first-class threading / reactions / media. Three concrete
  transports beyond Twilio, pick by deployment context: Meta's
  WhatsApp Cloud API (official business surface), WATI (BSP for
  regions where the cloud API isn't viable), and WhatsApp Web
  (unofficial, no business-account approval — fine for personal-use
  installs).
- **Email channel (SMTP/IMAP + Gmail push).** `[Z]` Deferred. Native
  email channel: SMTP outbound, IMAP IDLE inbound, with optional Gmail
  Pub/Sub push as a low-latency alternative to IMAP polling. Today Borg
  has no email path — users wanting inbound triage or outbound digests
  have to bridge via webhook plus their own MTA. Highest-reach channel
  gap in the current native set.

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

Each of these providers currently emits `cost_usd_present = false`
until pricing is wired into the static table path: **Groq**, **Together**,
**Fireworks**, **Bedrock**, **Vertex**, **xAI** (Grok). **Local models** stay
at `cost_usd_present = false` until a local-pricing policy exists.

### Tools & sub-agents

- **`delegate` subagent tool.** `[H]` Spawn a child agent with
  restricted toolset, fresh context, and depth/concurrency limits
  (`MAX_DEPTH=2`). Useful for "go research X" or "draft Y in parallel"
  without polluting main context. Distinct from workflows (which are
  sequential).
- **Server-initiated MCP sampling.** `[H]` If/when we add MCP client
  support, allow MCP servers to request completions back through Borg
  (full MCP spec, not just tool-calling).
- **Code-execution sandbox tool.** `[H]` Run user-provided Python (or
  Rust via `cargo script`) in a subprocess with a restricted stub
  library exposing only safe tools (web_search, read/write_file,
  search_files, patch, terminal). Resource caps on stdout/stderr/
  timeout/tool-calls. Useful for ad-hoc data crunching.
- **Cross-channel `send_message` tool.** `[H]` Deferred.
  Agent-callable tool for pushing a message to any configured
  channel (`{channel, recipient, body, thread?}`), independent of
  the channel the current turn came in on. Distinct from the
  rule-based "Channel mirror/relay" item under Streaming & channel
  delivery — that's config-driven forwarding; this is intentional,
  per-turn fan-out ("digest this to my email, ping the Slack
  channel, leave the Telegram thread quiet"). Today an agent can
  only respond on the channel that invoked it. Enforced by the
  existing `gateway.bindings` + `dm_policy` allowlists so the agent
  cannot reach senders the operator hasn't authorized.
- **Standalone `transcribe_audio` tool.** `[H]` Deferred.
  Agent-callable speech-to-text on arbitrary audio (file path, URL,
  or base64) via the existing `llm.aux` client (Whisper / Gemini
  STT). Distinct from the per-channel inbound voice transcription
  tracked under "Telegram voice follow-through" and from the Voice
  Call channel's realtime STT — those are channel-scoped; this is a
  general-purpose tool for voice memos, meeting recordings, and
  user-supplied audio attachments. Multimodal-in is currently
  asymmetric vs `text_to_speech` on the output side.

### MCP & external integrations

- **ACP — Agent Client Protocol (IDE backend).** `[Z]` Deferred.
  JSON-RPC 2.0 over stdio so Zed / VS Code / Cursor can use the agent
  as a coding-assistant backend. `cortex/codex_app_server` and
  `cortex/claude_cli.rs` exist as outbound CLI providers, but Borg
  does not expose an _inbound_ ACP server — no editor can drive Borg.
  Adding this turns Borg into a usable in-editor assistant alongside
  other coding-agent backends without giving up its background daemon role.
  Complement to the MCP server mode below (ACP is coding-backend
  surface; MCP server is state-introspection surface).
- **MCP support — both directions, native.** `[H][O]` Deferred. Adopt
  MCP (Model Context Protocol) as a first-class extensibility surface
  in addition to today's WASM plugins, both directions:
  Borg-as-server (`borgd serve-mcp`) so external IDEs / desktop
  clients (Claude Code, Cursor, and similar editors) drive Borg over JSON-RPC and
  can read/write sessions, messages, events, and permissions; and
  Borg-as-client so it consumes external MCP servers as
  agent-callable tools. Sibling of the ACP IDE-backend surface
  above (ACP is coding-backend, MCP server is state-introspection).
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
- **In-agent skill authoring workflow.** `[O]` Deferred. Today operators
  write `SKILL.md` files by hand at `~/.borg/skills/<name>/`. Add a
  guided flow that lets the agent itself scaffold a new skill: prompt
  for name + description, generate the YAML frontmatter, draft the body
  from a stated goal, validate required bins/env, and write the result
  under `~/.borg/skills/`. Complements the deferred `[I]`-tagged "Skill
  Workshop" (which captures _corrections_ into skills) — this is the
  _bootstrap-from-scratch_ surface, distinct from learning-from-feedback.

### Onboarding & migration

- **Modular setup wizard.** `[H]` Independently re-runnable sections:
  model picker → terminal/sandbox backend → channels → tools →
  credentials. Each section idempotent, invokable alone via
  `borg setup <section>`. Today onboarding is one linear flow.
- **Cross-tool migration importer (`borg migrate` / `borg import`).**
  `[H][I]` Bundled importers for Claude Code, Claude Desktop, and similar
  coding agents — pulls `AGENTS.md` / `CLAUDE.md` / hooks /
  approved-command lists / persona files / MCP servers / skills /
  command prompts / model providers / credentials into Borg's memory
    - config. Dry-run + overwrite presets. Removes the "redo your whole
      setup" tax for new users and lowers switching cost from other
      agents.

### Security & sandboxing

- **Windows sandbox — validate Tier 1 on a real host.** Tier 1
  (restricted token + DACL ACEs + `CreateProcessAsUserW`) was ported
  in 2026-05 and cross-compiles cleanly against the Windows target,
  but the FFI paths have never run. Spin up a Windows VM, run
  `cargo test -p sandbox --lib --target x86_64-pc-windows-msvc` (the
  `cfg(target_os = "windows")` tests in `sid::tests`,
  `process::tests`, and any new ones added during validation), then
  exercise the trampoline end-to-end: set `sandbox.enabled = true`,
  ask the agent to `run_shell` something that reads
  `~\.ssh\fake_file` (assert `Access is denied`) and that writes to
  the workspace (assert success). Until this is done, treat Tier 1
  as in-development and recommend operators run with
  `sandbox.enabled = false` if they hit issues.
- **Windows sandbox — Tier 2 elevated backend (WFP + sandbox users).**
  Deferred. Real network isolation (WFP kernel filter rules) and a
  separate user identity for the child require admin to set up.
  Design: `borg service install --windows-sandbox` triggers a UAC
  prompt, creates `BorgSandboxOffline` + `BorgSandboxOnline` users,
  registers WFP rules denying outbound for the offline SID, and
  persists state to `~/.borg/sandbox-windows/setup.json`. The
  trampoline auto-promotes when setup.json is present and version-
  current; auto-falls-back to Tier 1 otherwise. Mirrors the
  `security.kek_backend = auto` pattern.
- **Windows sandbox — caller authentication on the trampoline
  subcommand.** Deferred. The argv-policy transport has been replaced
  with an inheritable anonymous-pipe handoff (Phase 1 of the Codex
  parity work — see `crates/sandbox/src/windows/pipe.rs`), which kills
  the passive WMI / `tasklist /v` scrape path. The handoff still does
  not authenticate an *active* same-session same-integrity attacker
  who can `OpenProcess(PROCESS_DUP_HANDLE)` on the parent and grab a
  copy of the read-end. The Phase 7 elevated-runner named-pipe
  transport closes that gap by authenticating the caller via the
  pipe's security descriptor; until then, parent-PID validation or
  an inheritable shared-memory nonce would be incremental wins.
- **Windows sandbox — CI runner.** Deferred. Tests under
  `crates/sandbox/src/windows/` are gated `cfg(target_os =
"windows")` and only execute when developers run them on a Windows
  box. Add a `windows-latest` job to `.github/workflows/ci.yml`
  matrix once Tier 1 has been validated manually.
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
- **Browser tool — supervisor, anti-detection, persistent profiles.**
  `[H][O]` Deferred. Three related gaps in the current one-shot-fetch
  `browser` tool, sharing one supervisor: (1) automatic restart /
  reconnect on stale CDP sessions, crashed tabs, and disconnected
  browser processes so failures repair themselves without a manual
  restart; (2) anti-bot stealth headers + dialog auto-handling
  (current anti-detection posture is unverified); (3) persistent
  operator-scoped profiles (auth state, cookies, storage survive
  across calls), per-session tab management, and an "open this in my
  logged-in profile" action so multi-step authenticated workflows
  (book a flight, fill a form on a private dashboard) become
  possible.

### Observability

- **OpenTelemetry export pipeline.** `[O][I]` `[telemetry] otel_endpoint`
  setting; emit traces/metrics/logs for model calls, tool calls, exec,
  outbound HTTP, webhook processing, context-assembly, memory-pressure,
  and harness-lifecycle spans/metrics, with W3C traceparent propagation.
  Off by default. Today borg only exports OTel metrics for WASM plugins
  — this expands coverage across the agent loop. Targets power users
  running Borg as a long-lived service.
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

## Reference-survey `[I]` additions

Items from the 2026-05 survey of the fourth reference codebase. Ordered
roughly by ROI — each is independent.

### Gateway & ergonomics

- **OpenAI-compatible HTTP API.** `[I]` Expose `/v1/chat/completions`,
  `/v1/models`, `/v1/embeddings` on `borgd` so borg is a drop-in
  backend for any OpenAI client (Cursor, Continue, custom apps). Cheap
  to layer on top of the existing LLM stack; massive ecosystem reach.
- **Self-update command.** `[I]` `borg update` with a
  `BORG_NO_AUTO_UPDATE=1` kill switch for locked-down installs.

### Agent loop safety

- **Stuck-loop detection / unknown-tool guard.** `[I]` Default-on
  exponential backoff when the agent enters a tight tool-call loop,
  plus a guard that refuses hallucinated tool names instead of
  surfacing them as errors that the model then retries. Today a runaway
  model can burn an entire goal budget in seconds with no safety net.

### Voice & media

- **Voice Call channel.** `[I]` Twilio/Telnyx/Plivo realtime voice
  with streaming STT/TTS (Deepgram, ElevenLabs, OpenAI/xAI realtime).
  Borg has Twilio SMS but no voice path.
- **TTS/STT provider matrix.** `[I]` Grow beyond the single
  `text_to_speech` tool to a provider matrix — TTS: Edge,
  OpenAI, ElevenLabs v3, Gemini, Azure, Inworld, Volcengine, xAI,
  DeepInfra, MiniMax, local CLI, MLX; STT: Deepgram, ElevenLabs
  Scribe, Mistral, Whisper-local. Provider routing per channel/agent.

### Networking & deployment

- **Tailscale + mDNS/Bonjour discovery.** `[I]` Networked multi-device
  setup without manual cert + hostname wrangling. Pairs naturally with
  borg's mTLS-but-loopback-only constraint.
- **launchd / systemd integration.** `[I]` Full service lifecycle —
  install / uninstall / status / restart. Today `borg service install
--secure` only covers KEK setup.
- **APNs push pipeline.** `[I]` Wake disconnected iOS clients via a
  push relay with App Attest verification.
- **Tunneling.** `[I]` Bundled cloudflared/ngrok integration so users
  can expose the gateway without manual port-forwarding or reverse
  proxy setup.

### Observability

- **Prometheus exporter.** `[I]` Protected `/metrics` scrape route on
  `borgd` for operators running Prometheus stacks.
- **Stability snapshots / event-loop liveness.** `[I]` `/readyz` reports
  event-loop delay (p99/max), CPU ratio, and a `degraded` flag.
  Stability snapshots travel with diagnostic bundles.
- **Diagnostics export bundle.** `[I]` `borg diagnostics export`
  produces a sanitized logs / status / health / config bundle for
  support requests. Redacts secrets and absolute paths.

### Agent loop & LLM polish

- **Thinking modes.** `[I]` First-class `/think` surface with levels
  off / minimal / low / medium / high / xhigh / adaptive / max, mapped
  per provider (Gemini dynamic thinking, Anthropic adaptive, OpenAI
  flex). Per-model defaults overridable in config.
- **1M-context + `/fast` mode.** `[I]` Wire Anthropic `service_tier`,
  OpenAI fast variants, and the OpenAI 1M-context normalization through
  a user-visible `/fast` toggle with per-model defaults.
- **Multi-provider failover orchestrator.** `[I]` Sequential-on-retryable
  provider chain with per-provider cooldown — formalize the existing
  `llm.fallback` config into a documented orchestrator with structured
  error classification + per-provider cooldown so primary-provider
  auth, rate-limit, or overload incidents degrade gracefully to
  fallback providers / Claude CLI / Ollama instead of dead-ending the
  agent loop (verify current `llm.rs` first; some of this may already
  exist in the credential pool).
- **Compaction model override + mid-turn precheck + post-compaction
  workspace re-injection.** `[I]` Use a cheaper aux model for
  summarization, run compaction before the next tool call instead of
  end-of-turn, and re-inject workspace rules / project docs after
  compaction so the agent doesn't lose context discipline.
- **`models scan` free-model fallback.** `[I]` Discover OpenRouter
  free-tier models without an `OPENROUTER_API_KEY`.
- **`prompt_cache_key` opt-in + replay normalization.** `[I]` Per-provider
  opt-in for prompt cache keys, plus repair of orphaned reasoning,
  displaced tool results, and Anthropic/Bedrock thinking-signature
  stripping during replay.
- **URL allowlist for `web_search` / `web_fetch`.** `[I]` First-class
  HTTP egress restriction at the tool layer, matching
  `[security] blocked_paths` for files.
- **Session-management CLI.** `[I]` `borg agents`, `borg sessions`,
  `sessions export-trajectory`, `/subagents spawn` from chat.
- **Inferred follow-up commitments.** `[I]` Opt-in batched extraction
  of implicit user commitments ("I'll check in Friday") delivered via
  heartbeat. Per-agent / per-channel scoping.
- **Active Memory plugin.** `[I]` Dedicated memory sub-agent runs
  right before main reply, with configurable depth (message / recent /
  full) and partial-recall on timeout.
- **Skill Workshop.** `[I]` Captures reusable workflow corrections as
  pending or auto-applied workspace skills, threshold-based reviewer.
