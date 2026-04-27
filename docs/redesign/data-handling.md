# Data Handling — v1

Status: design, in active rework. Companion to [`architecture-redesign.md`](architecture-redesign.md), [`threat-model.md`](threat-model.md).

## Goal

Define how Borg retains, purges, and backs up agent data in v1. v1 is single-mode (no Sentry), no workspaces (Hermes-shaped), no per-workspace classification, no provider floor. The whole `BORG_HOME` directory is the unit of data — one tarball backs it up, one tombstone preserves audit-chain integrity on purge.

Workspace-level classification, per-workspace retention, provider attestation gates, KEK escrow, decommission attestation, and the `predicate_edited` audit anchor are all v3 additions tied to multi-tenancy and Sentry compliance.

## Non-goals

- **DLP on free-text content.** No regex-based "found a SSN, blocked the message." v1 has no classification system at all.
- **Cloud backup integration.** Daemon writes tarballs to local disk; user-side rsync/Time Machine/whatever handles offsite. No outbound backup transport.
- **NIST 800-88 media sanitization.** Tombstone-on-purge addresses retention only. v1 makes no sanitization claim.
- **Right-to-be-forgotten as a multi-user feature.** v1 is single-user; v3 multi-tenancy revisits.

## Locked decisions (v1)

| # | Decision |
|---|---|
| 1 | Retention is daemon-wide, single setting. Default = NULL (forever). |
| 2 | Purge of audit events = tombstone (preserve hash chain). Purge of messages/blobs = full delete. |
| 3 | Backup unit = the entire `BORG_HOME` directory as one tarball. |
| 4 | Tarballs are unsigned in v1 (single-user trust). Optional encryption via passphrase. |
| 5 | Restore into a daemon with conflicting state is refused unless `--force` (which renames the existing dir to `.bak`). |
| 6 | Hobbyist self-healing job runs `borg backup create` weekly, keeps last 3 by default in `~/.borg/backups/` and prunes older. Configurable 0–52 via `/settings → Data → Backups`. |
| 7 | Tombstone replaces `event_json` with `{purged: true, purged_at: ts}` keeping prev_hash + hash intact. |
| 8 | `borg backup verify` rebuilds the audit chain inside the tarball; failed verify = tarball not restorable. |

---

## Retention

### v1 default

Retention is global to the daemon. One setting:

```
[data]
retention_days = null    # default: forever
purge_policy   = "auto"  # "auto" | "on_demand_only" | "never"
```

Set via `/settings → Data → Retention`.

| Setting | Behavior |
|---|---|
| `retention_days = NULL` | Forever (default). |
| `retention_days = N` | Daily self-healing job purges anything older than N days, per `purge_policy`. |
| `purge_policy = "auto"` | Daily job runs purge. |
| `purge_policy = "on_demand_only"` | Daily job skipped; user runs `borg admin purge --before <date>` manually. |
| `purge_policy = "never"` | Purge entirely disabled even when retention_days is set. |

### Retention enforcement

Self-healing daily task (already exists for log pruning) extended to scan the global retention setting. For `cutoff = now - retention_days * 86400`:
- Messages with `created_at < cutoff` → full delete from `messages` table.
- Audit events with `ts < cutoff` → tombstone (see below).
- Blobs referenced by purged messages → delete from disk.
- Provider call events older than cutoff → tombstone.

(v3 adds per-workspace retention with `retention_floor_days` from classification predicates.)

---

## Tombstone-on-purge

Audit chain integrity demands hash continuity. Purge of an audit event replaces the event payload with a tombstone marker, keeping the hash:

```sql
UPDATE evolution_events
SET event_json = json('{"purged": true, "purged_at": ' || ?1 || ', "original_type": "' || event_type || '"}'),
    event_purged = 1
WHERE id = ?2;
```

The `prev_hash` and `hash` columns stay untouched. `borg audit verify` re-derives the hash from the *current* `event_json` and compares against the stored `hash`. After tombstoning, the recomputed hash will mismatch — which is the *correct* behavior for verification: the auditor sees "event existed at this seq; payload was purged at <ts>; original hash was <stored>." A verifier flag distinguishes purge from tampering:

```rust
pub enum AuditVerifyOutcome {
    Match,                          // hash recomputes
    PurgedTombstone { original_hash: String, purged_at: i64 },
    Tampered { recomputed: String, stored: String },
}
```

`PurgedTombstone` is a non-failure status. `Tampered` is the actual failure.

**Why not crypto-erase.** Crypto-erase (per-`BORG_HOME` key, "purge" = destroy key) is cleaner cryptographically but adds a key-management subsystem v1 doesn't have. Tombstone is sufficient for audit retention semantics: the *audit metadata* (who/when/what tool) is what's load-bearing; the *tool output* is what tombstones cover.

---

## Backup & restore

### Backup unit

One unit in v1: **the entire `BORG_HOME` directory as a single tarball.** Contains: SQLite, blobs/, audit chain, configuration, internal CA + paired-device cert metadata. mTLS private keys live in the OS keychain and are *not* in the tarball — by design (a backup tarball should not be an mTLS-key exfil channel).

(v3 splits into per-workspace backup units + a separate system backup unit when workspaces exist.)

### Backup commands

```
borg backup create
  → ~/.borg/backups/<timestamp>.tar.zst

borg backup create --passphrase-from <fd>
  → encrypts the tarball with the supplied passphrase

borg backup verify <file>
  → reads tarball, rebuilds audit chain, asserts continuity, exits 0/1

borg backup restore <file> [--force]
  → atomic restore; refuses if BORG_HOME has existing state, --force renames
    existing dir to .bak.<timestamp>
```

Self-healing job runs `borg backup create` weekly, keeps the **last 3 by default** in `~/.borg/backups/` and prunes older. The retention count is configurable via `/settings → Data → Backups` (range 1–52); set to `0` to disable auto-backup. `/status` exposes the current count and the oldest retained tarball's timestamp so the user sees pruning happen rather than discovering it after a loss. No cloud, no offsite. Disk-full = warning surfaced via doctor, not silent backup failure.

### Backup verification

`borg backup verify <file>` extracts to a temp dir, opens the SQLite, runs `borg audit verify` against the embedded audit chain, and asserts:

- HMAC chain continuous (or only `PurgedTombstone` breaks).
- Schema version supported.
- All blob refs in messages exist as files in `blobs/`.

Failures: refuses to mark the tarball restorable. User gets a per-check failure list.

### Restore semantics

Refused if:
- A `BORG_HOME` with existing state is present (use `--force` to rename existing → `.bak.<ts>`).
- The tarball signature fails verification (when signed; v1 unsigned).
- The schema version is newer than the daemon's max-supported.

### Restore audit

Every restore writes a `state_restored` event into the audit log of the *new* daemon state — first event of the restored chain — recording: source filename, sha256, restored-by principal, into-`BORG_HOME` path.

---

## Schema migrations on hash-chained tables

The hard rule: **migrations may add columns and write new events; they may never rewrite existing event payloads.** Adding a column to `messages` is fine. Backfilling `evolution_events.event_json` to add a new field is forbidden.

### Migration test posture

Every migration runs `borg audit verify` as a post-check; if it fails, migration rolls back. CI runs the migration suite against a hash-chained fixture DB and asserts chain integrity at every version step.

### Versioning

Backup tarballs include `schema_version` in `metadata.json`. Daemon refuses to restore a tarball whose `schema_version > daemon.schema_version`. Forward-compat: tarballs with `schema_version < daemon.schema_version` migrate-on-restore.

---

## UX surfaces

| Surface | v1 |
|---|---|
| `/settings → Data` | Retention controls; backup count (default 3, range 0–52); "Backup now" button |
| `/status` | "Data" row: spent disk + oldest backup ts + retention setting |
| Backup completion | Toast: *"Backed up — 3 tarballs retained."* |
| Restore | Wizard prompts for `--force` confirmation if existing state present |

---

## Test posture

- **Retention purge.** Auto-purge job removes messages older than `retention_days`; corresponding audit events tombstone; chain verifies as `PurgedTombstone`, not `Tampered`.
- **Tombstone integrity.** Manually tampered tombstone (e.g., setting `event_purged = 0` while leaving payload tombstoned) detected by verify as `Tampered`.
- **Purge policy `never` blocks purge.** Setting `purge_policy = "never"` keeps everything regardless of `retention_days`.
- **Backup roundtrip.** Create + verify + restore on a fresh daemon yields identical state (messages, audit chain hash heads, blobs).
- **Backup encryption.** `borg backup create --passphrase-from <fd>` produces an encrypted tarball; restore without the passphrase fails; with the passphrase succeeds.
- **Conflict-on-restore.** Restore into existing `BORG_HOME` refused; same tarball restored with `--force` renames existing to `.bak.<ts>` and proceeds.
- **Schema migration chain integrity.** Run all migrations against a hash-chained fixture; assert `borg audit verify` passes at every step.
- **Cross-version restore.** Tarball at schema v3 restored into v5 daemon migrates on restore; reverse refused.

---

## Deferred to v3 (multi-tenancy + Sentry)

When v3 lands, this doc gains:

- **Workspace classification** — `Public` / `Internal` / `CUI` / `SBU` / `Custom("...")` columns on `workspaces`, with classification predicates table for custom labels.
- **Per-workspace retention** — `workspaces.retention_days`, mandatory in Sentry mode at workspace creation, agency-policy-driven (1y / 3y / 7y common per NIST AU-11).
- **Provider floor enforcement** — workspace classification × provider attestation matrix; hard refusal in Sentry, warning in default mode.
- **Per-workspace backup unit** — separate from system backup unit; daemon-level state (paired devices, listeners, system audit chain) becomes its own backup unit.
- **Tarball signing** — Sentry-mode backups signed with a daemon backup key separate from server cert.
- **Mandatory backup encryption (Sentry)** — refuses cleartext output, cites MP-4 / SC-28.
- **KEK escrow** — M-of-N Shamir over the KEK with threshold matching admin quorum, sealed to admin certs.
- **`predicate_edited` audit anchor** — every classification predicate mutation appends a chain event with content-hash digest; doctor flags drift as `PredicateTampering`.
- **`borg admin decommission`** — produces an attested deletion record naming NIST 800-88 explicitly to disclaim sanitization.
- **Restore-with-rename** — workspace restore into a new ID for conflict resolution.

The v1 schema (no `workspaces` table, no `classification_predicates`, no `kek_shares`) is intentionally minimal. v3 adds tables and columns; v1 audit chain stays compatible.

---

## Open questions (v1)

1. **Encrypted-by-default backups.** Should v1's auto-backup encrypt with a daemon-generated passphrase stored in the OS keychain? Lean no for v1 (single-user, "your disk, your problem"); revisit if backups end up syncing to iCloud / Drive automatically.
2. **Litigation-hold semantics.** When a user sets `purge_policy = "on_demand_only"`, retention timer should pause. Current design: the timer doesn't pause; user must extend retention manually. Adequate for v1.

---

## References

- Companion: [`architecture-redesign.md`](architecture-redesign.md) — Layer 4d audit chain
- Companion: [`auth-and-onboarding.md`](auth-and-onboarding.md) — system audit chain, listener floors
- Companion: [`threat-model.md`](threat-model.md) — tombstone vs crypto-erase reasoning
- Existing audit chain code: `crates/core/src/db/migrations.rs`, `crates/core/src/evolution/replay.rs`
- NIST SP 800-53 Rev 5: AU-9, AU-11, MP-6 (v3 mapping)
- NIST SP 800-88 Rev 1 (media sanitization — v3 disclaimer)
