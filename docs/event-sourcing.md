# Events & Logs as First-Class State

> **Thesis.** The durable, append-only log is the agent. Everything else —
> in-memory state, the agent loop, the TUI, the JSONL files — is a
> *projection* over that log. If the process bursts into flames, you restart
> it, replay the log, and the agent is exactly where it was.

This is the same inversion databases went through: the write-ahead log is
primary; the materialized tables are a cache you can always rebuild. Borg is
already most of the way there. This document is the plan to finish the job —
to make events and logs first-class rather than emergent, **with full test
coverage and no behavioral regressions**.

---

## 1. Where Borg already lives the thesis

Borg did not start from a mutable-state design, so this is a consolidation,
not a rewrite. Five surfaces are *already* event-sourced:

| Surface | Table | Chain | Projection | Source |
|---|---|---|---|---|
| Conversation | `messages` (`event_kind`/`event_json`/`hmac`/`prev_hmac`, added in `migrate_v8`) | per-session SHA-256 | `conversation::project_history()` → working history | `crates/cortex/src/conversation/event.rs` |
| Vitals | `vitals_events` (append-only triggers) | SHA-256, domain `borg-vitals-chain-v1` | `vitals::replay_events_with_key()` → 5 stats | `crates/cortex/src/vitals.rs`, `db/vitals.rs` |
| Bond | `bond_events` | SHA-256, domain `borg-bond-chain-v1` | `bond::replay_events_with_key()` → score/autonomy | `crates/cortex/src/bond/` |
| Evolution | `evolution_events` | SHA-256 + Ed25519, domain `borg-evolution-chain-v1` | `evolution::replay_events_with_key()` → stage/level/XP | `crates/cortex/src/evolution/` |
| Audit | `audit_log_chain` | blake3, tombstone-purge | `verify_audit_chain()` | `crates/cortex/src/db/audit_chain.rs` |

Each is the canonical pattern: `BEGIN IMMEDIATE` → read previous link →
compute new link → INSERT, never UPDATE/DELETE; read = replay from baseline
with per-link integrity verification. **No mutable state table.** Vitals
literally has DB triggers that `ABORT('append-only')` on UPDATE/DELETE.

Compaction is already modeled as an event (`SessionEventKind::CompactionCheckpoint`),
not a destructive rewrite — the raw rows stay, the checkpoint just changes the
projection. That is exactly the thesis in miniature.

## 2. Why it is not yet *first-class*

Three gaps separate "we use event sourcing in five places" from "the log is
the agent."

### Gap A — Five ledgers, zero shared abstraction

Each ledger hand-rolls the identical machinery: writer-serialized append,
previous-link read, chain computation, baseline, ordered replay, per-link
verify, cascade-skip on tamper, hourly rate limiting. That is ~4,000 lines of
near-duplicate code across `vitals.rs`, `bond/`, `evolution/`, `audit_chain.rs`,
and `conversation/event.rs`. Two different hash primitives are in play
(SHA-256 vs blake3) for no principled reason. A bug fixed in one replay loop
is not fixed in the others. Adding a sixth ledger means copying a fourth
implementation.

The shared helpers that *do* exist (`hmac_chain::compute_hmac`,
`verify_chain_link`, `HourlyRateLimiter`, `Database::derive_hmac_key`) prove
the pattern is uniform — they just stop short of owning the append/replay loop.

### Gap B — Runtime events are broadcast-only, not logged

Four in-memory buses fan events out to the TUI and gRPC subscribers:

| Bus | Type | Capacity | Durable? |
|---|---|---|---|
| Heartbeat | `broadcast::Sender<DaemonHeartbeatEvent>` | 32 | **No** |
| Goals | `broadcast::Sender<DaemonGoalEvent>` | 32 | side `goals` table only |
| Agent-tree | `broadcast::Sender<AgentTreeEvent>` | 256 | side snapshot tables |
| Cost | `broadcast::Sender<CostEvent>` | 1024 | optional export |

These are real events, but the broadcast channel *is* the truth: on daemon
restart the stream's history is gone, a late subscriber cannot replay what it
missed, and a dropped/lagged receiver silently loses events. This is the
inverted-from-thesis design — the bus is primary and persistence is an
afterthought, rather than the log being primary and the bus being its live
tail.

### Gap C — The JSONL log is a parallel hand-rolled side-channel

`crates/cortex/src/logging.rs` writes `~/.borg/logs/{date}.jsonl` with a
hand-rolled, fire-and-forget appender invoked right next to every
`persist_message()` call. It duplicates data already in the `messages` event
log, can silently diverge from it, has its own redaction pass, and is not
derived from anything. A log file should be a *rendering of the event log*,
not a second place we hope stays in sync.

## 3. The design

Two unifications and one demotion. The guiding principle is the blog's
closing line: **the loop becomes an executor over the log, and everything
else is a projection.**

### 3.1 One ledger primitive (`cortex::ledger`)

Introduce a single generic append-only, integrity-chained log and express
every existing ledger as a thin instance. New module
`crates/cortex/src/ledger/` with three small traits:

```rust
/// A domain that stores its history as an append-only chained log.
pub trait Ledger {
    /// Stable wire identity for HMAC key derivation, e.g. b"borg-vitals-chain-v1".
    const DOMAIN: &'static [u8];
    /// Physical table name, e.g. "vitals_events".
    const TABLE: &'static str;
    /// One log entry. Serialized to the row's typed columns / event_json.
    type Event: LedgerEvent;
    /// The materialized view this log projects to.
    type State;
    type Projection: Projection<Event = Self::Event, State = Self::State>;

    /// Optional per-category write/replay caps. Default: unlimited.
    fn rate_policy() -> RatePolicy { RatePolicy::unlimited() }
}

/// Pure, deterministic fold. No clock, no IO — replayable anywhere.
pub trait Projection {
    type Event;
    type State;
    fn baseline() -> Self::State;
    fn apply(state: &mut Self::State, event: &Self::Event, ctx: &ReplayCtx);
}

/// What a row needs to chain + (de)serialize. The chained byte layout is
/// derived from these so a new ledger cannot forget a field.
pub trait LedgerEvent: Sized {
    fn chain_fields(&self) -> Vec<&[u8]>; // hashed in order after prev_hmac
    fn to_row(&self) -> LedgerRow;
    fn from_row(row: &LedgerRow) -> Result<Self>;
}
```

The engine owns the parts that are identical everywhere — and only those:

```rust
impl Database {
    /// Append: BEGIN IMMEDIATE, read prev link, compute, INSERT. One impl.
    pub async fn ledger_append<L: Ledger>(&self, event: &L::Event) -> Result<Appended>;

    /// Replay: ordered scan, per-link verify, cascade-skip tamper, fold via
    /// L::Projection with rate limiting from L::rate_policy(). One impl.
    pub async fn ledger_state<L: Ledger>(&self) -> Result<L::State>;

    /// Stream raw verified events (for tailing / projections that aren't State).
    pub async fn ledger_events<L: Ledger>(&self, from_seq: i64) -> Result<Vec<L::Event>>;
}
```

Vitals/bond/evolution/audit/session each shrink to: an `Event` struct, a
`Projection` impl (the `apply` reducer — the *only* domain-specific logic),
and a `RatePolicy`. The append/replay/verify/cascade/serialize loops collapse
into one tested core. Elegance falls out for free: a ledger is now "a reducer
over a verified log," which is precisely the thesis.

Hash primitive is unified on HMAC-SHA256 (the majority). Audit's blake3 chain
is migrated behind the same interface (see §5 — additive, old rows verify
under a legacy verifier so existing chains stay valid).

### 3.2 A durable event spine (`cortex::events`)

Make the log primary for runtime events too. A single durable `events` ledger
(a `Ledger` instance from §3.1) is the canonical record of every domain event
— heartbeat fired, goal set/cleared, sub-agent spawned, cost incurred. The
in-memory broadcast bus becomes the **live tail of that log**, not a parallel
truth:

```rust
pub struct EventSpine {
    db: Database,                       // durable log (source of truth)
    tx: broadcast::Sender<EventEnvelope>,// live tail (projection of the append)
}

impl EventSpine {
    /// Append-then-notify. The broadcast is best-effort; the log is not.
    pub async fn emit(&self, ev: DomainEvent) -> Result<Seq> {
        let appended = self.db.ledger_append::<EventLog>(&ev.into_row()).await?; // durable first
        let _ = self.tx.send(EventEnvelope { seq: appended.seq, ev }); // tail; ok if no subs
        Ok(appended.seq)
    }

    /// Late/reconnecting subscriber: replay missed tail from a seq cursor,
    /// then switch to live. No gaps across restarts.
    pub async fn subscribe_from(&self, cursor: Seq)
        -> impl Stream<Item = EventEnvelope>;
}
```

This directly fixes Gap B: a TUI that reconnects after a daemon restart
replays from its last seen `seq`; a lagged subscriber recovers from the log
instead of losing events; the daemon's whole event history is inspectable
after the fact. The existing typed buses (`HeartbeatBus`, `GoalEventBus`,
`AgentTreeBus`, `CostEventBus`) become thin typed *views* that filter the one
spine — preserving every current `Admin.Subscribe*` gRPC surface unchanged.

### 3.3 Logs as a projection, not a sink

Demote `logging.rs` from a writer to a renderer. The `{date}.jsonl` file
becomes a **projection of the event spine + session log**, produced by a
single subscriber that tails the durable log and renders rows (applying the
existing `OutputRedactionPolicy`). There is then exactly one source of truth;
the file can be deleted and regenerated by replaying the log. This fixes Gap C
and removes the "write to DB, then separately hope the JSONL matches" coupling
at every `log_and_persist()` call site.

```
event spine (durable)  ──tail──▶  jsonl renderer  ──▶  ~/.borg/logs/{date}.jsonl
        │
        └──tail──▶  broadcast views (TUI, gRPC streams)
```

## 4. What this buys us (and what we explicitly do *not* do)

**Buys:** one tested append/replay core instead of five; durable, replayable,
gap-free runtime events; logs that cannot diverge from truth; trivial future
ledgers (a reducer + a rate policy); "rebuild any view by replaying the log"
as a real, testable property.

**Out of scope (deliberately):** We do **not** make the `sessions` *metadata*
row event-sourced — cumulative tokens/title/lease are a legitimate mutable
cache rebuildable from the log, and the lease mechanism is load-bearing for
the worker pool. We do **not** add snapshotting/checkpointing for replay
performance in this pass (current event volumes replay in <1ms; revisit only
if a profile says so). We do **not** change any event's semantics, deltas,
caps, or wire formats — this is a structural consolidation, so existing chains
stay valid and existing subscribers see identical streams.

## 5. Migration & regression safety

The whole plan is **additive and parity-gated**. No existing table is dropped
or rewritten in place; no chain is invalidated.

1. **Land the engine alongside the old code.** Add `cortex::ledger` with the
   generic append/replay. Do not touch callers yet.
2. **Port one ledger at a time, behind a parity test.** For each of
   vitals → bond → evolution → audit → session, implement the `Projection`
   and assert byte-for-byte that the generic engine produces the *same*
   chained HMAC and the *same* replayed state as the legacy implementation
   over a corpus of recorded fixtures. Only delete the legacy impl once parity
   is green. (Audit's blake3 rows verify under a `LegacyBlake3` verifier so
   pre-migration chains remain valid forever.)
3. **Introduce the spine as a superset.** Stand up `EventSpine` and have the
   existing buses publish *through* it while still serving their current gRPC
   streams. Snapshot tests assert each `Admin.Subscribe*` stream is unchanged.
4. **Flip logs to a projection last.** Replace the inline `log_message()` calls
   with the tail renderer; assert the rendered JSONL is identical (modulo
   ordering guarantees) to today's output over a fixture session.
5. **Schema:** one new migration (`migrate_v10`) adds the `events` table only.
   No `ALTER` to existing event tables. `CURRENT_VERSION` bumps 9 → 10.

If any parity test cannot be made green, that ledger stays on its legacy path
— the design tolerates partial adoption, so a hard case never blocks the rest.

## 6. Test strategy (full coverage)

Per the project's no-fluff-tests invariant, every test must be able to fail
for a real reason. Coverage targets:

- **Engine unit tests** (`ledger/`): append computes the correct chain link;
  replay folds in order; a tampered row sets `chain_valid = false` and
  cascade-skips the tail (port the existing `test_tampered_event_skipped_in_replay`
  / `test_mid_chain_corruption_skips_tail` cases to the generic engine);
  rate-policy caps drop at write and attenuate at replay; baseline is
  clock-independent (replay is deterministic given the same events).
- **Parity tests** (one per ported ledger): legacy vs generic produce
  identical HMAC chains and identical projected state over recorded fixtures.
  These are the regression firewall — they *must* exist before any legacy
  deletion.
- **Spine tests**: `emit` is durable-before-broadcast (kill the broadcast,
  assert the row persisted); `subscribe_from(cursor)` replays exactly the
  missed events across a simulated restart with zero gaps and zero dups; a
  lagged subscriber recovers from the log.
- **Log-projection tests**: rendered JSONL equals the legacy writer's output
  over a fixture session; redaction is still applied; deleting and
  regenerating the file from a log replay yields the same bytes.
- **gRPC stream snapshot tests**: each `Admin.Subscribe*` stream is
  byte-identical before/after the spine swap.
- **Migration test**: `migrate_v10` is idempotent and a v9 DB with existing
  vitals/bond/evolution/audit chains still verifies after upgrade.

CI continues to gate at 80% line coverage (`cargo llvm-cov --fail-under-lines 80`);
the engine consolidation should *raise* coverage by collapsing five lightly-
tested loops into one heavily-tested one.

## 7. File map

| Concern | New / changed |
|---|---|
| Generic ledger engine | **new** `crates/cortex/src/ledger/{mod,append,replay,row}.rs` |
| Event spine | **new** `crates/cortex/src/events/{mod,spine,envelope}.rs` |
| Ledger instances | refactor `vitals.rs`, `bond/`, `evolution/`, `db/audit_chain.rs`, `conversation/event.rs` to `impl Projection` |
| Durable runtime events | rewire `borgd/src/{heartbeat,goal_bus,cost_events}.rs` + `cortex/src/multi_agent/event_bus.rs` to publish through the spine |
| Logs as projection | demote `crates/cortex/src/logging.rs` to a spine tail renderer |
| Schema | `migrate_v10` in `crates/cortex/src/db/migrations.rs`; bump `CURRENT_VERSION` |

---

**In one sentence:** Borg already stores its conversation, vitals, bond,
evolution, and audit history as append-only chained logs — this plan unifies
those five hand-rolled loops into one tested ledger engine, makes runtime
events durable-first with the broadcast bus as a replayable tail, and turns
the JSONL log into a projection of that single source of truth, so that the
log truly *is* the agent.
