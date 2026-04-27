# Task 05 — Posture v2 + data handling + cleanup

> Read first: [`security-posture.md`](../security-posture.md), [`data-handling.md`](../data-handling.md).
> Depends on: Tasks 01–04 (final pass over the rewritten daemon).

## Goal

Three loosely-coupled finishing pieces that don't justify their own task each:

1. **Posture v2** — collapse the existing 5-bucket posture ladder into a 3-state model (`Reduced` ×0.85 / `Default` ×1.00 / `Boosted` ×1.10) with schema-stubbed forward-compat to the v3 5-bucket ladder.
2. **Data handling** — daemon-wide retention with tombstone-on-purge for audit chain integrity; single-tarball backup unit.
3. **Cleanup pass** — delete dead code from the in-process era, sweep stale docs, ensure cross-references in `docs/redesign/` still resolve, ensure CLAUDE.md still describes the shipped system.

## Scope

### Posture v2

Replace the current 5-bucket evolution posture model in `crates/core/src/evolution/` with:

```rust
enum Posture { Reduced, Default, Boosted }

impl Posture {
    fn is_reduced(&self) -> bool { matches!(self, Posture::Reduced) }
    fn is_boosted(&self) -> bool { matches!(self, Posture::Boosted) }
    fn xp_multiplier(&self) -> f32 {
        match self {
            Posture::Reduced => 0.85,
            Posture::Default => 1.00,
            Posture::Boosted => 1.10,
        }
    }
}
```

Schema:

```sql
CREATE TABLE posture_snapshots (
  id INTEGER PRIMARY KEY,
  taken_at INTEGER NOT NULL,
  posture TEXT NOT NULL,           -- Reduced | Default | Boosted
  reason TEXT NOT NULL,            -- which signals fired
  schema_version INTEGER NOT NULL DEFAULT 2  -- v3 flips to 3 for 5-bucket ladder
);
```

- Per-event XP cap stays at 3 (do not raise to 4 — that's v3).
- New milestone: `posture_boosted_first` (one-shot).
- Posture transitions emit a system cell + an audit chain event (Task 02).
- Existing 5-bucket data: migrate by mapping `Hardened`/`Strict` → `Boosted`, `Default` → `Default`, `Permissive`/`Risky` → `Reduced`. Keep old rows readable.

### Data handling

Per `data-handling.md`:

- `[retention]` config block, daemon-wide. Defaults: `messages = NULL` (forever), `audit_log = NULL`, `task_runs = 90d`, `embedding_cache = 30d` (unchanged).
- Purge job runs daily (extend the existing maintenance task at 02:00).
- **Tombstone-on-purge.** When purging audit chain rows, replace each with a `tombstone` row (same `seq`, `event_type = "tombstoned"`, `payload_json = {original_event_type, purged_at, retention_policy_id}`, recompute `hash` correctly). `borg audit verify` distinguishes:
  ```rust
  enum AuditVerifyOutcome { Ok, Tampered(seq), PurgedTombstone(seq) }
  ```
- Backups: `borg backup create` produces a single `borg-backup-<ts>.tar.zst` of `$BORG_HOME` (DB + memory + skills + channels + hooks). `borg backup restore <path>` is destructive; warn + require `--yes`.
- Default 3 backups kept (configurable 0–52). Older backups pruned by daily maintenance.
- Backup signing, KEK escrow, decommission attestation — all v3.

### Cleanup pass

- Delete in-process agent code paths from `crates/cli` that were replaced by gRPC client code in Task 01. Anything in `crates/core` only consumed by deleted CLI paths goes too.
- `crates/gateway` likely deletable after Task 03 — confirm and remove.
- Doc sweep:
  - Re-grep `docs/` for cut concepts (`workspace`, `sentry`, `SCIM`, `FIPS`, `Hardened`, `Strict`, `Permissive`, `KEK escrow`, `Derived PIV`, `classification_predicate`). Anything outside `docs/redesign/` mentioning them is stale — fix or delete.
  - Verify every internal link in `docs/redesign/*.md` still resolves.
  - Update `README.md` and top-level `docs/` index to point at the daemon + gRPC architecture.
- CLAUDE.md sweep:
  - "Architecture" crate table needs a `borgd` row, possibly removes `gateway` (if folded into daemon).
  - "Critical Invariants" stays verbatim — no changes.
  - Add a brief "gRPC kernel" section under Systems Reference describing the daemon/client split.
  - Update any commands that changed (e.g. `borg pair`, `borg revoke`, `borg providers refresh-rates`, `borg backup create/restore`, `borg audit verify`).

## Out of scope

- 5-bucket posture ladder, streak-based milestones, per-event weights — v3.
- Per-workspace retention policies, classification-tagged retention — v3.
- Backup KEK escrow, tarball signing, decommission attestation — v3.

## Done when

- All posture-related tests pass against the 3-state model; old 5-bucket rows readable; new `posture_boosted_first` milestone fires once.
- Retention purge job runs and produces tombstones; `borg audit verify` returns `Ok` on a chain with tombstones (distinguishes from `Tampered`).
- `borg backup create` then `borg backup restore` round-trips a daemon's full state on a fresh machine.
- Dead code removed: `cargo build` clean, no `#[allow(dead_code)]` snuck in to silence warnings.
- `grep -R "Hardened\|Strict bucket\|workspace" docs/` returns only intentional v3-deferral mentions.
- CLAUDE.md describes the v1-as-shipped architecture; running an agent against the freshly-built tree from a clean checkout produces no surprises.
- `cargo fmt --check && cargo clippy -- -D warnings && cargo test` clean across the workspace.

## Risks to watch

- **CLAUDE.md drift.** The instructions file is load-bearing for future sessions. After this task, re-read it end-to-end against the actual repo state.
- **Tombstone hash continuity.** A tombstone replacing seq N must keep the chain intact for seq N+1. The recomputed hash uses the tombstone payload, not the purged payload. Test the verify path explicitly.
- **Backup completeness.** OS-keychain-held mTLS keys are NOT in `$BORG_HOME` — backups won't restore them. `borg backup restore` on a new host triggers a re-pair flow. Document this clearly.
