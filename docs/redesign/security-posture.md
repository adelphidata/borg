# Security Posture & XP Multiplier — v1

Status: design, in active rework. Companion to [`evolution.md`](../evolution.md) and [`auth-and-onboarding.md`](auth-and-onboarding.md).

## Goal

Bind XP accrual rate to the security posture of the install. Turning off security knobs hurts XP velocity; deliberately hardening helps. v1 is a deliberately simple **3-state** model — Reduced / Default / Boosted — sized to the actual security knobs that exist in v1.

The 5-bucket graduated ladder from prior drafts (Lax / Baseline / Reinforced / Fortified / Hardened) is deferred to v3, where Sentry-mode adds enough additional dimensions (vault primary, classifications, hardware key floors, attestation, FIPS) to make the ladder meaningful. v1 has fewer knobs, so v1 has fewer states.

## Non-goals

- **Stage-gate coupling.** Posture does not affect stage transitions, bond, vitals, mood, or `AutonomyTier`. Posture is strictly a *rate* multiplier.
- **Retroactive recompute.** Changing live config never alters historical XP. Posture history lives in the HMAC chain; only forward events feel a change.
- **Per-channel weight calibration.** All channel listeners contribute neutral 1.0× per-event weight in v1.
- **WASM/dynamic posture predicates.** State resolution is a compiled-in pure function.

## Locked decisions (v1)

| # | Decision |
|---|---|
| 1 | Three states: `Reduced` / `Default` / `Boosted`. Multipliers: ×0.85 / ×1.00 / ×1.10. |
| 2 | `Reduced` wins on any single deliberate downgrade. `Boosted` requires the full Boosted-signal set. Otherwise `Default`. |
| 3 | Posture changes are events (`security_posture_changed`) appended to the existing HMAC chain alongside `xp_gain`, `mood_changed`, etc. |
| 4 | Replay reads posture *only* from the chain, never from live config. Live config changes only affect future events. |
| 5 | Full `PostureSnapshot` written per event (no deltas). Corruption stays local. |
| 6 | Posture events are rate-limited to **2/hr**. Excess events drop, prior posture continues for replay (anti-flip-flop). |
| 7 | Per-event cap stays at **3 XP**. (The cap-raise to 4 was scoped to Hardened in the 5-bucket ladder; v1's smaller multiplier range doesn't need it.) Round-half-up. |
| 8 | Channel listeners contribute 1.0× per-event weight in v1. |
| 9 | Greenfield — first event in every chain is an install-time bootstrap. |
| 10 | UX: no ambient header. `/evolution` posture line. `/xp` shows posture + weekly avg multiplier as a header. `/card` unchanged. Toast on `security_posture_changed`. |
| 11 | Failure mode (posture undetermined at write time): fall back to 1.0× Default with `fallback` recorded in event metadata. |
| 12 | One milestone: `posture_boosted_first` (first ever transition into Boosted). v3 adds the streak-based ladder. |

---

## Architecture

### The posture event in the HMAC chain

Posture is a first-class event type in `evolution_events`, hash-chained the same way as `xp_gain`, `mood_changed`, and the rest. The chain is the single source of truth — replay never reads listener state from live config when computing historical XP.

| Event type | XP Δ | Description |
|---|---|---|
| `security_posture_changed` | 0 | Full posture snapshot. Metadata: `PostureSnapshot` JSON. |

Replay maintains a running `current_posture: Option<PostureSnapshot>`. On each `security_posture_changed`, update. On each `xp_gain`, compute multiplier from `current_posture`. Before the first posture event in the chain, `current_posture = None` and `xp_gain` events use 1.0× fallback.

### Emit points

The daemon emits `security_posture_changed` on:

1. `SettingsResolver` set/unset of any tracked security key (sandbox enabled, blocked_paths, channel `allow_privileged`, listener config).
2. Listener bind/unbind.
3. **Daemon-startup reconciliation.** On every daemon start, compute live posture, compare against chain's most recent `security_posture_changed`. If they differ, write a new event before serving traffic.

### Rate limiting

`security_posture_changed: 2/hr` joins the existing rate-limit table. When the limit triggers, the event is dropped — replay sees the *prior* posture continue. Closes the flip-flop farming attack: an adversary toggling Boosted ↔ Reduced to time it with high-XP actions hits the cap and gets stuck at one effective transition per 30 minutes.

### State resolution

```rust
fn resolve_state(p: &PostureSnapshot) -> PostureState {
    if is_reduced(p) { return Reduced; }
    if is_boosted(p) { return Boosted; }
    Default
}

// Reduced: ANY ONE deliberate downgrade. Each is a config knob the user must
// explicitly flip; none happens by accident.
fn is_reduced(p: &PostureSnapshot) -> bool {
    !p.global.sandbox_enabled
    || !p.global.privileged_channels.is_empty()
    || !p.global.blocked_paths_strict
    || !p.global.catastrophic_denylist_intact
    || p.listeners.iter().any(|l| {
        matches!(l.bind, ListenerBindKind::PublicTofu)
            && l.auth_backend == AuthBackend::None
    })
}

// Boosted: every signal that can be turned on in v1, IS on.
fn is_boosted(p: &PostureSnapshot) -> bool {
    p.global.sandbox_enabled
        && p.global.audit_chain_enabled
        && p.global.blocked_paths_strict
        && p.global.privileged_channels.is_empty()
        && p.global.catastrophic_denylist_intact
        // every non-loopback listener has real auth
        && p.listeners.iter().all(|l| {
            matches!(l.bind, ListenerBindKind::Loopback)
                || l.auth_backend != AuthBackend::None
        })
        // at least one listener uses mTLS or Tailscale (i.e., the daemon
        // does more than just talk to localhost)
        && p.listeners.iter().any(|l| {
            matches!(l.auth_backend, AuthBackend::Tofu | AuthBackend::Tailscale)
        })
}
```

State → multiplier:

| State | Multiplier | Reachability |
|---|---|---|
| `Reduced` | ×0.85 | Any single deliberate downgrade |
| `Default` | ×1.00 | Fresh install. Loopback only, sandbox on, audit chain on, no privileged channels, denylist intact |
| `Boosted` | ×1.10 | All Default conditions + at least one mTLS/Tailscale listener (daemon is reachable from at least one paired remote device or tailnet) |

The Boosted condition is deliberately reachable on a normal v1 install: pair a phone (well, in v1: a second laptop) over Tailscale or PAKE, keep the sandbox on, don't tamper with the denylist, don't grant a channel `Privileged`. That's the design — Boosted should feel like the natural reward for using the redesign as intended.

### Multiplier math

```
1. base_xp           = action_table.lookup(tool)            // 0, 1, 2 (existing)
2. with_alignment    = base_xp + align_bonus(archetype)     // 0..3 (existing)
3. with_security     = with_alignment × state_multiplier
4. clamped           = clamp(round_half_up(with_security), floor, 3)
                       where floor = 1 if with_alignment > 0 else 0
5. rate_limit_check  = if xp_gain rate cap exceeded { drop } else { write_xp_gain(clamped) }
```

State multipliers:

| State | Multiplier |
|---|---|
| Boosted | ×1.10 |
| Default | ×1.00 |
| Reduced | ×0.85 |

Pacing target: a sustained-Boosted user reaches stage gates ~10% faster than Default. Sustained-Reduced is ~15% slower. v1 has no per-event weight — listener kind doesn't add or subtract. (v3's 5-bucket ladder reintroduces per-event weights when Sentry-flavored signals exist.)

---

## Schema

### `PostureSnapshot` (in `security_posture_changed.metadata_json`)

```rust
#[derive(Serialize, Deserialize)]
pub struct PostureSnapshot {
    pub schema_version: u32,           // start at 1
    pub state: PostureState,           // resolved at write time
    pub global: GlobalSignals,
    pub listeners: Vec<ListenerPosture>,
    pub computed_at: i64,              // unix ts; debugging only
    pub computed_from: PostureSource,  // ConfigChange / ListenerBind / DaemonStartReconcile
}

#[derive(Serialize, Deserialize)]
pub struct GlobalSignals {
    pub sandbox_enabled: bool,
    pub blocked_paths_strict: bool,            // matches default-or-stricter
    pub privileged_channels: Vec<ChannelId>,   // any allow_privileged=true
    pub audit_chain_enabled: bool,             // hash-chained audit log enabled
    pub catastrophic_denylist_intact: bool,    // catastrophic-command list unmodified
}

#[derive(Serialize, Deserialize)]
pub struct ListenerPosture {
    pub name: String,                          // e.g. "loopback", "tailnet", "telegram"
    pub bind: ListenerBindKind,                // Loopback / Tailnet / PublicTofu / Channel
    pub auth_backend: AuthBackend,             // None / Tailscale / Tofu
    pub max_tier: Tier,
}

#[derive(Serialize, Deserialize)]
pub enum PostureState { Reduced, Default, Boosted }
```

**Rationale:**
- `state` stored, not derived at replay. If state-derivation rules tighten in a future version, historical events keep their original state — replay determinism preserved.
- Full snapshot per event (not deltas). Storage is trivial at realistic posture-event rates (~10/year).
- `listeners` is a `Vec`, not a `HashMap`. Stable serialization order is critical for HMAC reproducibility.
- `schema_version` enables forward compat. v3's 5-bucket ladder bumps to v2; v1 events stay valid.

### `xp_gain` metadata extension

```rust
pub struct XpGainMetadata {
    pub schema_version: u32,
    pub archetype_aligned: bool,
    pub bonus_reason: Option<String>,
    pub tool: Option<String>,

    pub xp_breakdown: XpBreakdown,
    pub posture_event_seq: i64,        // evolution_events.id of the
                                       // governing security_posture_changed
}

pub struct XpBreakdown {
    pub base: u32,                     // 0/1/2
    pub align_bonus: u32,              // 0 or 1
    pub state_multiplier: f32,         // 0.85 / 1.0 / 1.10
    pub state: PostureState,
    pub pre_clamp: f32,
    pub clamped: f32,
    pub final_xp: u32,
    pub fallback: Option<FallbackReason>,
}

pub enum FallbackReason {
    PostureUndetermined,
    PostureChainBroken,
    SchemaVersionUnknown,
}
```

`posture_event_seq` is the audit cross-reference. `borg audit verify` reads an `xp_gain`, fetches the referenced `security_posture_changed`, recomputes `state → multiplier → final_xp`, and asserts equality.

---

## UX surfaces

| Surface | Behavior |
|---|---|
| Ambient TUI header | **No change.** Posture is not in the header. |
| `/evolution` | Posture line near the top: `Posture: Boosted (×1.10)` etc. |
| `/xp` | Header/subheader shows current posture + weekly avg multiplier (`Posture: Default ×1.00  ·  Avg this week: ×1.00`). |
| `/card` | **No change.** |
| Posture-change toast | One-shot `System` cell on every `security_posture_changed`: `Posture: Default → Boosted. Future XP earns at ×1.10.` |

Discovery is opt-in: `/evolution` and `/xp` are deliberate-attention surfaces. Users who never type them never see posture, but also never feel pressured.

---

## Failure modes

**Posture undetermined at `xp_gain` write time** (very early daemon startup, chain corruption):
- Write the `xp_gain` event with `final_xp = base + align` (no multiplier), `state = Default`, `fallback = Some(reason)`.
- Continue serving traffic. Never pause XP, never queue events.
- Doctor sweep surfaces a `persistent_warning` if fallback events appear in two consecutive runs.

**Posture chain corruption** (most recent `security_posture_changed` fails HMAC verification):
- Replay skips the broken event per evolution.md's existing rule.
- Falls back to the previous valid `security_posture_changed`.

**Out-of-band config edit while daemon stopped:**
- Daemon-startup reconciliation detects divergence between live config and chain's most recent posture.
- Emits a new `security_posture_changed` with `computed_from = DaemonStartReconcile` *before* serving traffic.

---

## Migration

Greenfield. No legacy compat needed.

On first daemon start after v1 lands:
1. Compute current posture from live config.
2. Write the bootstrap `security_posture_changed` event with `computed_from = DaemonStartReconcile`.
3. All subsequent `xp_gain` events use the multiplier as designed.

---

## Test posture

- **State resolution table.** One row per `(global_signals, listeners) → expected state`. Covers Reduced-wins-on-conflict, all individual Reduced disqualifiers, the Boosted reachability path (loopback + tailnet), and the Default fallback.
- **Multiplier math.** Aligned creation under Boosted produces 3 XP (cap honored). Single tool-call under Reduced produces 1 XP (floor honored, not 0).
- **Replay determinism.** Inject a fixed sequence of posture + xp_gain events; replay produces identical XP totals across runs.
- **HMAC chain integrity.** Mutating a posture event row breaks the chain; replay skips it and falls back to previous snapshot.
- **Rate limit anti-flip-flop.** 3 posture changes within an hour: third drops, replay computes against second posture for events written during the dropped-event window.
- **Migration bootstrap.** Daemon startup with no prior posture events writes the bootstrap.
- **Doctor warning escalation.** Two consecutive doctor runs with fallback events present surface a persistent warning.

---

## Implementation notes

### Crates / files (sketch)

- `crates/cortex/src/evolution/security/mod.rs` — `PostureSnapshot`, `PostureState`, `XpBreakdown`, public types.
- `crates/cortex/src/evolution/security/resolver.rs` — `resolve_state`, `is_reduced`, `is_boosted`.
- `crates/cortex/src/evolution/security/snapshot.rs` — read live config + listeners → `PostureSnapshot`.
- `crates/cortex/src/evolution/security/emit.rs` — write `security_posture_changed` events with rate-limiting; daemon-startup reconciliation.
- `crates/cortex/src/evolution/replay.rs` — extend running state with `current_posture`; apply multiplier to `xp_gain` replay.
- `crates/cortex/src/evolution/feed.rs` — extend `/xp` header rendering with posture + weekly avg multiplier.
- `crates/cortex/src/evolution/commands.rs` — extend `/evolution` rendering with posture line.
- `crates/interface/src/tui/app.rs` — surface `security_posture_changed` toast as a `System` cell.

---

## Deferred to v3

When Sentry-mode lands and v3 introduces vault-as-primary, workspace classifications, hardware key tiers, FIPS, attestation, and per-workspace provider floors — *then* the simple 3-state model can no longer differentiate enough installs. v3 reintroduces:

- The 5-bucket graduated ladder (`Lax` / `Baseline` / `Reinforced` / `Fortified` / `Hardened`).
- The streak-based `posture_hardened_*` milestone series (30d, 60d, 90d, 1y, annual).
- Per-event weight contributions for tailnet/sentry listeners and Tier 3 actions.
- The cap raise from 3 XP to 4 XP under Hardened.
- Schema bump from `PostureState` enum to `PostureBucket`.

The `schema_version` field on `PostureSnapshot` exists exactly to enable this without breaking v1 chains.

---

## Open questions (v1)

1. **`borg audit verify` posture cross-check.** Should the verifier re-resolve states from chain inputs and confirm `xp_breakdown.state` agrees? Lean yes, with a `--strict` flag for "fail the audit on resolver drift" vs. default warn-only.
2. **`/heal` on posture chain corruption.** When the doctor detects a broken `security_posture_changed` chain link, should `/heal` offer to write a re-reconciliation event (advancing past the broken span) or only warn? Lean warn-only.

---

## References

- Companion: [`evolution.md`](../evolution.md) — XP curves, archetypes, milestones, HMAC chain.
- Companion: [`auth-and-onboarding.md`](auth-and-onboarding.md) — listener model, tier ceilings.
- Companion: [`architecture-redesign.md`](architecture-redesign.md) — Layer 4 security model, audit chain.
- Existing replay: `crates/cortex/src/evolution/replay.rs`
- Existing milestone dedup: `crates/cortex/src/evolution/milestones.rs`
- Existing rate-limit table: `crates/cortex/src/evolution/mod.rs`
