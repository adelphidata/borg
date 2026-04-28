# Roadmap

Tracked follow-ups from shipped work. Each entry names the source plan or
commit that deferred the work so context isn't lost.

## Pre-launch hardening

Items uncovered by the 2026-04 pre-launch audit. Cleared items move out;
unfinished items stay here so a future contributor can see what's left.

- **Shell-policy bypass surface.** `crates/cortex/src/policy.rs` now
  recovers escapes, backticks, and literal `$(cmd)` substitutions before
  the deny recheck (commit `df6705a`). Substitutions whose value is
  computed at runtime (`$(echo rm) -rf /`) still slip past — the policy
  is documented best-effort and the sandbox is the real boundary, but a
  future shell-words-based parser would close the residual gap.
- **TOCTOU on blocked-path checks.** `is_blocked_path()` resolves full
  symlink chains via `canonicalize()`; a regression test pins the
  behavior (commit `54fbc3b`). The window between check and open is
  acknowledged in the threat model. A defense-in-depth pass using
  `O_NOFOLLOW` on Linux opens or `/proc/self/fd` re-canonicalization
  would close it; deferred until there's a credible exploit path.
- **CI coverage gate.** CLAUDE.md targets 80%+ but `.github/workflows/`
  has no failing threshold. Add `cargo llvm-cov --fail-under-lines 80`
  once the daemon session executor and scheduler grow real coverage,
  or soften the doc claim.
- **Nightly `--include-ignored` job.** `crates/cortex/src/browser/mod.rs`
  and the `borgd_launcher` integration tests are `#[ignore]`'d behind
  Chrome and daemon-spawn requirements. A nightly CI lane with those
  prerequisites would catch first-run regressions before users do.

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
