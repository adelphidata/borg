# Roadmap

Tracked follow-ups from shipped work. Each entry names the source plan or
commit that deferred the work so context isn't lost.

## Self-healing — Tier 2 / Tier 3

Deferred from the "Tier 1 — core self-healing" plan (see `docs/self-healing.md`
and commit `e666f4c`). Tier 1 shipped missed-run detection, the memory size
cap, and the daily maintenance sweep. Tier 2 then closed the items below
except where noted:

- ~~**Skill audit log / unknown-skill detection.**~~ Shipped. V39 migration
  added `skill_audit` and the doctor Skills check compares SHA-256 of each
  user `SKILL.md` against the stored hash, warning on divergence. TOFU on
  first load. See `docs/self-healing.md` "Skill tamper audit".
- ~~**Gateway async-blocking fixes.**~~ Shipped. iMessage monitor now loads
  manifest + state with `tokio::fs` via `ChannelManifest::load_async`; the
  sync `load()` is retained for the registry scanner.
- ~~**Persistent doctor-warning TUI banner.**~~ Shipped as a one-shot
  startup notice (not a persistent banner — explicit design choice to
  avoid user fatigue). `App::new` queries `latest_doctor_run` and pushes
  a `System` cell after the opening card when `persistent_warnings` is
  non-empty; scrolls away like any other message.
- **CVE advisory integration.** Deferred. Pulling rustsec / OSV advisories
  on a cadence would mean an outbound git/HTTP fetch from a background
  task, which is unwelcome on airgapped or locked-down deployments and
  adds non-trivial binary weight for a feature many installs won't use.
  Revisit if user demand materializes — a `cargo audit` wrapper around
  `Cargo.lock` in the release binary is one safer path.

## Self-healing — known gaps worth tracking

All three post-Tier-1 gaps are now shipped. See `docs/self-healing.md`
for current behavior:

- ~~**Stuck `running` task_runs while daemon stays up.**~~ The 5-min
  daemon tick calls `recover_wedged_runs`, which fails any `running`
  row whose `started_at` is older than the task's `timeout_ms`
  (falling back to `STALLED_TASK_GRACE_SECS`).
- ~~**Clock-jump storm.**~~ `heal_stalled_tasks` collapses audit rows
  into one aggregate entry when more than
  `CLOCK_JUMP_AGGREGATE_THRESHOLD` (5) tasks drift in a single sweep.
- ~~**Timezone drift on seeded crons.**~~ `calculate_next_run` honors
  IANA names on the `timezone` column via `chrono-tz`. `"local"`,
  `""`, and `"UTC"` still map to UTC so existing seeded rows keep
  their original semantics; unparseable zones fall back to UTC with
  a `tracing::warn!`.

## /btw — non-blocking side questions

Shipped (TUI-only): `/btw <question>` spawns a tool-less, non-persistent side agent that answers using the current session's transcript snapshot. Result renders in a dismissable modal popup. See `crates/core/src/agent/btw.rs` and `crates/cli/src/tui/btw_popup.rs`.

Deferred:

- **Gateway / channel surface.** Mirror `run_btw` into `crates/gateway/src/handler.rs` so Telegram, Slack, Discord, iMessage, etc. can handle a `/btw ` prefix before routing to the main agent. Reply should land as a threaded follow-up so it doesn't derail the main conversation.
