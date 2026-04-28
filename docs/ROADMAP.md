# Roadmap

Tracked follow-ups from shipped work. Each entry names the source plan or
commit that deferred the work so context isn't lost.

## Self-healing — Tier 2 / Tier 3

- **CVE advisory integration.** Deferred. Pulling rustsec / OSV advisories
  on a cadence would mean an outbound git/HTTP fetch from a background
  task, which is unwelcome on airgapped or locked-down deployments and
  adds non-trivial binary weight for a feature many installs won't use.
  Revisit if user demand materializes — a `cargo audit` wrapper around
  `Cargo.lock` in the release binary is one safer path.

## /btw — non-blocking side questions

- **Gateway / channel surface.** Mirror `run_btw` into
  `crates/gateway/src/handler.rs` so Telegram, Slack, Discord, iMessage,
  etc. can handle a `/btw ` prefix before routing to the main agent.
  Reply should land as a threaded follow-up so it doesn't derail the
  main conversation.
