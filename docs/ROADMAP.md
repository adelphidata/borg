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

## Browser CDP attach

The `/browser connect|disconnect|status` slash command and the matching
`borg browser …` CLI let the agent attach to a user's live Chromium over
Chrome DevTools Protocol and share that profile/cookies instead of always
launching a fresh headless instance. Two follow-ups were intentionally
deferred:

- **Cloud / managed-browser providers.** Borg has no `BrowserProvider`
  trait + plugin registration today — the browser tool only supports
  local-headless launch and local-CDP-attach. A future pass should add a
  provider trait, wire it through the WASM/MCP plugin surfaces, and let
  `/browser` toggle between local-headless, local-attached-CDP, and
  remote managed endpoints (e.g. browserless-style cloud browsers).
  Deferred from the initial `/browser` CDP-attach pass.
- **TUI browser pane / screenshot overlay.** Today `/browser` only emits
  sys-messages into the transcript. A future pane that renders the live
  page would need to coexist with the mouse-interaction invariant in
  `CLAUDE.md` (no `?1000h`/mouse-capture escapes in the inline viewport),
  so it's a non-trivial design problem rather than a drop-in. Deferred
  from the initial `/browser` CDP-attach pass.

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
    1. **Cerebras.** Wafer-scale inference, fastest tokens/sec for
       Llama/Qwen; ideal `llm.aux` target where latency dominates quality.
    2. **Together AI / Fireworks.** Broad OSS-model hosting
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

### Memory provider — full prompt-block routing

The v1 `MemoryProvider` trait ships routing for the three model-visible
memory tools (`write_memory`, `read_memory`, `memory_search`) and the
per-turn `<active_memory>` block, but the stable-prefix
`<long_term_memory>` block still loads from local SQLite regardless of
provider (`load_memory_context_db_scoped_with` in
`crates/cortex/src/memory/mod.rs`). When `memory.provider = mem0|honcho|
openviking`, external-provider content reaches the model via the
per-turn search path; the prefix block is empty until a fresh
consolidation populates the local store. Closing this means adding a
`MemoryProvider::prompt_block(max_tokens) -> String` method (Native
delegates to the existing scoped loader; externals format their own
hits into `<memory_entry>` XML), caching the result per turn so the
HTTP cost doesn't compound with system-prompt cache misses, and
wiring `agent/prompt_assembly.rs::load_memory*` to dispatch through it.

### Memory providers — additional integrations

The pluggable `MemoryProvider` trait ships with three external backends
(`mem0`, `honcho`, `openviking`) plus the default `native` SQLite store.
The following candidates were considered for v1 and deferred — each is
a single-file impl under `crates/cortex/src/memory/provider/` plus a
`MemoryProviderKind` variant, modelled on the three that already ship.

- **Hindsight.** Hybrid local + cloud knowledge graph with multi-strategy
  retrieval (vector, BM25, graph traversal) and a `reflect` endpoint
  that synthesizes across hits. Tools surface as `retain` / `recall` /
  `reflect` rather than write/read/search; mapping onto Borg's existing
  three model-visible tools needs an extra metadata field per write.
- **Holographic / HRR-based stores.** Entity-resolution-first stores
  (holographic reduced representations, fact graphs with trust scoring
  + temporal decay). Adds an entity-resolution dependency and a more
  expansive tool surface (`add` / `search` / `probe` / `related` /
  `reason` / `contradict` / `update` / `remove` / `list`). Worth
  revisiting if knowledge-graph memory becomes a measured win over the
  hybrid vector+BM25 native path.
- **Self-hosted vector DB adapters.** Direct adapters to Qdrant /
  Weaviate / Pinecone / pgvector would let an operator point Borg at
  an existing org-wide vector store rather than the bundled SQLite
  embeddings. Lower-level than the current external providers — those
  are agent-memory services that do their own embedding; this would
  be embeddings-only with Borg's existing chunker + ranker reused.
- **Letta (MemGPT) / Zep.** Two well-known agent-memory services not
  surveyed in v1. Mem0 and Honcho cover overlapping ground; revisit if
  operator demand surfaces specifics neither bundled provider covers
  (e.g. Letta's hierarchical memory levels, Zep's enriched session
  summaries).

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
- **Mixture-of-agents tool.** `[H]` Deferred. Query multiple models in
  parallel and synthesize a single answer. High cost; reserved for
  high-stakes single-turn use cases. Niche — defer until explicit
  demand.
- **Structured clarify tool.** `[H]` Deferred. Multi-choice clarification
  with structured options the operator picks from, distinct from
  `request_user_input` (free-form). Marginal value; likely covered by
  extending `request_user_input` with an `options` parameter rather than
  adding a new tool.
- **Post-write lint / diagnostics on write + patch.** `[H] ref`
  Deferred. After every `fs_write` / patch apply, run a fast
  language-aware check (syntax/lint for Python, JSON, YAML, TOML, Rust)
  and fold any failure back into the tool result so the agent
  self-corrects on the next turn instead of discovering the breakage
  later. Today writes land unchecked — a malformed JSON config or
  unparseable edit only surfaces when something downstream reads it.
- **Stale-file staleness guard on whole-file writes.** `[H] ref`
  Deferred. Track the on-disk mtime/hash of each file the agent reads
  and refuse (or warn on) a subsequent blind `fs_write` when the file
  changed underneath since that read — the classic read-modify-write
  clobber. Complements the patch DSL, which already fails on drifted
  context anchors but doesn't protect a full-file overwrite.
- **Per-turn file-mutation verifier footer.** `[H] ref` Deferred.
  Append a compact "files created / modified / deleted this turn"
  summary (computed from the workspace diff) to the agent's turn
  output. Gives the operator an at-a-glance audit of what changed
  without diffing, and gives the agent a self-check that its writes
  matched intent.
- **`/steer` mid-run nudge + `/queue` prompt queue.** `[H] ref`
  Deferred. `/steer <text>` injects guidance the running agent picks up
  after its next tool call without interrupting the loop; `/queue
  <text>` stacks follow-up prompts to run when the current turn
  finishes. Borg injects `<proactive_nudges>` internally but exposes no
  operator-facing steer/queue surface — today the only way to redirect
  a busy agent is to interrupt it.

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
- **Standalone `transcribe_audio` tool.** `[D]` Done for local files.
  Agent-callable speech-to-text now accepts a local audio path and uses
  the same `audio.models` fallback chain as gateway voice transcription,
  including xAI/Grok, OpenAI/Groq Whisper, Mistral, Deepgram, local, and
  command-backed providers. URL/base64 ingestion remains deferred until
  it can share the normal URL policy and media-size gates.
- **Background-process watch patterns + completion notify.** `[H] ref`
  Deferred. `run_shell` already has persistent-session `start` / `poll`;
  add fire-and-forget background execution that pushes a notification on
  process exit and on lines matching operator-supplied `watch_patterns`
  (e.g. `ERROR`, `Listening on`) instead of forcing the agent to
  poll. Turns long builds and dev servers into event-driven waits.
- **Office-document text extraction.** `[H] ref` Deferred. `fs_read`
  extracts PDF text via `pdf_extract` today; extend the same path to
  DOCX / XLSX / PPTX so the agent can read the Office attachments
  operators routinely drop in. Highest-value file types not yet
  parseable.
- **Persistent multi-agent task board with auto-decomposition.**
  `[H] ref` Deferred. A durable kanban/swarm surface that
  auto-decomposes one task into a dependency tree of sub-tasks,
  dispatches them across worker sub-agents, and self-heals
  (stale-running detection, worker reclaim, respawn-storm guard, cycle
  detection in the decomposition). Distinct from per-session goals
  (single Ralph loop) and one-shot `delegate` — this is a long-lived
  board with parent/child promotion and concurrency caps. Large
  surface; folds into the existing `spawn_supervised` + `multi_agent`
  machinery.
- **`video_generate` tool.** `[H] ref` Deferred. Borg has
  `video_analyze` (Gemini inline-data) but no generation path; a
  `video_generate` tool with pluggable provider backends would mirror
  the deferred image-gen provider routing. Niche; defer until demand.

### MCP & external integrations

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
- **Multi-instance profiles (export / import / clone).** `[H] ref`
  Deferred. Run several isolated Borg identities from one install —
  separate data dir, credentials, sessions, and gateway bindings per
  profile — with `borg profile export/import/clone/rename` for moving
  or templating an instance. Today one install is one identity
  (`~/.borg/`); operators wanting a "work" vs "personal" split must
  juggle data dirs by hand.
### Security & sandboxing

- **Windows sandbox — real-host validation pass (code complete).**
  Tier 1 (restricted token + DACL ACEs + `CreateProcessAsUserW`,
  anonymous-pipe `SpawnRequest` IPC, job-object
  `KILL_ON_JOB_CLOSE` containment, `ProcThreadAttributeList`
  mitigation policies, per-spawn isolated desktop, cap SIDs, audit
  events) AND Tier 2 (elevated runner + sandbox accounts + WFP +
  ConPTY, below) are now fully implemented and cross-compile clean to
  `x86_64-pc-windows-gnu`; the `windows-sandbox` CI lane runs the
  `windows_ffi_smoke` integration tests (real `CreatePipe` /
  `CreateDesktopW` / `CryptProtectData` / `CreatePseudoConsole` /
  `UpdateProcThreadAttribute(PSEUDOCONSOLE)` / WFP round-trip when
  elevated) on every PR. The **only** remaining gap is hands-on
  enforcement validation on a real Windows VM with admin — it cannot
  run on GH-actions runners (no admin, no usable HKCU, no
  pre-provisioned accounts). Until that pass happens, treat the
  sandbox as in-development and recommend operators run with
  `sandbox.enabled = false` if they hit issues. Validation checklist:
  - Tier 1: `sandbox.enabled = true`, no `setup.json`; `run_shell`
    reading `~\.ssh\fake_file` returns `Access is denied`, writing to
    the workspace succeeds; an interactive `run_shell` drives the
    ConPTY path while a captured (piped) call uses inherited stdio
    unchanged.
  - Tier 2: `borg service install --windows-sandbox` (elevated)
    creates `BorgSandboxOnline`/`BorgSandboxOffline`, seals their
    DPAPI creds, installs the WFP filters, writes `setup.json`;
    spawns route through the runner; under Offline `curl example.com`
    fails (WFP), under Online it succeeds; deny-path read returns
    `Access is denied`. `borg service uninstall --windows-sandbox`
    removes accounts, filters, creds, and `setup.json`.
  - Then strike the "in-development" caveat and recommend
    `sandbox.enabled = true` as the default.
  - Follow-up (defense-in-depth, non-blocking): zeroize the
    sandbox-account password buffers (`provision::generate_account_password`
    output, the unsealed `String` in `runner_client::load_account_password`,
    and the wide buffer passed to `CreateProcessWithLogonW`) so the
    plaintext doesn't linger on the heap after use.

  Implemented in this pass (all cross-compile-validated against the
  `x86_64-pc-windows-gnu` target):
  - `borg service {install,uninstall} --windows-sandbox` clap
    subcommand → `borgd_launcher` → `elevated::install` (elevation
    check, account provisioning, DPAPI-sealed creds, WFP install,
    atomic `setup.json` write).
  - `elevated::provision` — `NetUserAdd` / `NetLocalGroupAddMembers` /
    LSA `SeBatchLogonRight` + deny interactive/network logon, SID
    resolution, cross-platform strong-password generator.
  - `elevated::runner_client` / `runner_pipe` — operator-side
    `CreateProcessWithLogonW` launch of the runner *as the sandbox
    account* (privilege-correct vs. the original
    `LogonUserW`+`CreateProcessAsUserW` plan, which needs caller
    privileges a non-admin operator lacks), SDDL-scoped named pipe,
    `GetNamedPipeClientProcessId` caller authentication, and the
    `ipc_runner` framed protocol (`SpawnRequest`/`Output`/`Stdin`/
    `Resize`/`Exit`/…, base64 binary, 8 MiB cap).
  - `wfp` — operational `FwpmEngineOpen0` + transactional
    `FwpmProviderAdd0` / `FwpmSubLayerAdd0` / `FwpmFilterAdd0` ×2
    (V4+V6) with an `ALE_USER_ID` security-descriptor condition for
    the offline SID, `PERSISTENT | BOOTTIME`, and delete-by-key
    teardown; filter *specs* are cross-platform unit-tested.
  - `runtime::run_inner` Tier1↔Tier2 routing via `mode::detect()`
    with fail-safe fallback to the Tier 1 anonymous-pipe self-exec on
    any Tier 2 error.
  - ConPTY wired into `process::spawn_with_token` via a `SpawnStdio`
    enum (`Inherit` / `Handles` / `Pty`): `Pty` adds the
    `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` attribute and drops
    `STARTF_USESTDHANDLES`; `conpty::pump_parent_stdio` copies bytes
    via std threads; `ResizePseudoConsole` exposed. The trampoline
    uses ConPTY only when stdout is a real console, so non-interactive
    tool calls keep the proven inherited-handle path.
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
- **External secrets-manager backends.** `[H] ref` Deferred.
  `${SECRET:NAME}` resolves today via credential store → env → OS
  keychain. Add optional resolvers that read from Bitwarden Secrets
  Manager, 1Password, or HashiCorp Vault (lazy-install the vendor CLI,
  cache hits with a TTL) so operators with an existing org secrets
  store don't have to duplicate credentials into Borg's keychain. Slots
  in behind the existing `resolve_credential_or_env` seam.

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

### Voice & media

- **Voice Call channel.** `[I]` Twilio/Telnyx/Plivo realtime voice
  with streaming STT/TTS (Deepgram, ElevenLabs, OpenAI/xAI realtime).
  Borg has Twilio SMS but no voice path.
- **TTS/STT provider matrix.** `[I]` Borg has a shared
  `audio.models`/`tts.models` provider matrix for local voice, gateway
  voice, `text_to_speech`, and `transcribe_audio`: Edge, OpenAI,
  ElevenLabs, Gemini, MiniMax, Mistral, xAI/Grok, Piper, local, and
  command-backed local engines. Remaining work: Azure/Inworld/
  Volcengine/DeepInfra/ElevenLabs Scribe and per-channel provider
  routing.

### Networking & deployment

- **Tailscale + mDNS/Bonjour discovery.** `[I]` Networked multi-device
  setup without manual cert + hostname wrangling. Pairs naturally with
  borg's mTLS-but-loopback-only constraint.
- **APNs push pipeline.** `[I]` Wake disconnected iOS clients via a
  push relay with App Attest verification.

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
