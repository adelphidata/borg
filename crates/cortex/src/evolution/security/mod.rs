//! Security posture — 3-state model (`Reduced` / `Default` / `Boosted`).
//!
//! Posture is the XP-rate multiplier that binds gameplay to the security
//! configuration of an install.
//!
//! This module contains:
//! - [`PostureState`] and the multiplier table (`xp_multiplier`).
//! - [`PostureSnapshot`], the JSON payload persisted into
//!   `evolution_events.metadata_json` for `security_posture_changed` rows.
//! - [`XpBreakdown`], the per-`xp_gain` audit trail that lets
//!   `borg audit verify` re-derive the final XP from posture inputs.
//! - The pure [`resolver`] (Reduced-wins-on-conflict, Boosted requires the
//!   full positive set) used by both write-time emission and replay.

pub mod emit;
pub mod resolver;
pub mod snapshot;

use serde::{Deserialize, Serialize};

pub use emit::{current_posture, maybe_emit_posture_change, EmitOutcome};
pub use resolver::{is_boosted, is_reduced, resolve_state};
pub use snapshot::compute_live_snapshot;

use crate::db::Database;

/// Read the head of `posture_snapshots` for write-time XP multiplier
/// resolution. Returns `(state, posture_event_seq, fallback)` — the seq is
/// the `evolution_event_id` (audit_log_chain.seq in v1) of the
/// `security_posture_changed` row that established this state.
///
/// On a fresh DB with no posture row yet, returns
/// `(Default, 0, Some(PostureUndetermined))` so `xp_gain` writes still pass
/// through `compute_xp_breakdown` and surface the fallback in their
/// `XpBreakdown` for `borg doctor` escalation.
pub fn current_posture_for_xp(db: &Database) -> (PostureState, i64, Option<FallbackReason>) {
    let row: Option<(String, i64)> = db
        .conn()
        .query_row(
            "SELECT posture, evolution_event_id FROM posture_snapshots
             ORDER BY taken_at DESC, id DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .ok();
    match row.and_then(|(s, seq)| PostureState::parse(&s).map(|p| (p, seq))) {
        Some((state, seq)) => (state, seq, None),
        None => (
            PostureState::Default,
            0,
            Some(FallbackReason::PostureUndetermined),
        ),
    }
}

/// Per-event XP cap. Stays at 3 in v1; v3's 5-bucket ladder may raise it
/// under `Hardened`. Mirrored from `evolution::MAX_XP_DELTA` so resolver
/// tests can reference it without pulling the whole module.
pub const MAX_XP_DELTA: u32 = 3;

/// 3-state posture model.
///
/// Order is `Reduced < Default < Boosted` along the multiplier axis but the
/// enum is *not* `Ord` — comparisons must go through `xp_multiplier`. v3 will
/// extend this to 5 buckets behind a `schema_version` bump on
/// [`PostureSnapshot`]; v1 chains stay valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PostureState {
    /// Any single deliberate downgrade is in effect.
    Reduced,
    /// Fresh-install defaults: loopback only, sandbox on, audit chain on,
    /// no privileged channels, denylist intact.
    Default,
    /// Default conditions plus at least one mTLS or Tailscale listener.
    Boosted,
}

impl PostureState {
    /// XP rate multiplier applied at write time to `(base + align_bonus)`.
    /// Stored in [`XpBreakdown::state_multiplier`] for replay verifiability.
    pub fn xp_multiplier(&self) -> f32 {
        match self {
            PostureState::Reduced => 0.85,
            PostureState::Default => 1.00,
            PostureState::Boosted => 1.10,
        }
    }

    /// Stable identifier used in event payloads and the
    /// `posture_snapshots.posture` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            PostureState::Reduced => "Reduced",
            PostureState::Default => "Default",
            PostureState::Boosted => "Boosted",
        }
    }

    /// Parse from the stable identifier; returns `None` on unknown values
    /// rather than panicking, so an old daemon reading a v3-bumped chain can
    /// fall back to `Default` with a `SchemaVersionUnknown` reason.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Reduced" => Some(PostureState::Reduced),
            "Default" => Some(PostureState::Default),
            "Boosted" => Some(PostureState::Boosted),
            _ => None,
        }
    }
}

/// Snapshot of the security knobs that determine [`PostureState`] at write
/// time. Written as JSON into `evolution_events.metadata_json` for the
/// `security_posture_changed` row, and indexed by `posture_snapshots`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostureSnapshot {
    /// Schema version. v1 = `1`. v3's 5-bucket ladder bumps to `2`.
    pub schema_version: u32,
    /// State resolved from `global` + `listeners` at write time. Stored
    /// rather than re-derived on replay so future tightening of resolver
    /// rules doesn't rewrite history.
    pub state: PostureState,
    pub global: GlobalSignals,
    pub listeners: Vec<ListenerPosture>,
    /// Unix seconds; debugging only — the hash chain is the audit root.
    pub computed_at: i64,
    pub computed_from: PostureSource,
}

/// Globally-scoped signals (not tied to any single listener).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalSignals {
    pub sandbox_enabled: bool,
    /// True when `[security] blocked_paths` matches the default-or-stricter
    /// set. False when the user explicitly weakened it.
    pub blocked_paths_strict: bool,
    /// Channel ids with `allow_privileged = true`. Empty in default install.
    pub privileged_channels: Vec<String>,
    pub audit_chain_enabled: bool,
    /// True when the catastrophic-command denylist (`rm -rf /`, `mkfs`,
    /// `dd if=/dev/zero`, `curl|sh`) is unmodified.
    pub catastrophic_denylist_intact: bool,
}

/// Per-listener posture-relevant facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenerPosture {
    /// Stable name used for UX (`"loopback"`, `"tailnet"`, `"telegram"`).
    pub name: String,
    pub bind: ListenerBindKind,
    pub auth_backend: AuthBackend,
}

/// Where a listener accepts connections from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListenerBindKind {
    /// UDS or `127.0.0.1` only.
    Loopback,
    /// Tailscale-attested peer set.
    Tailnet,
    /// Public TCP with TOFU mTLS.
    PublicTofu,
    /// Webhook / channel ingress (Telegram, Slack, …).
    Channel,
}

/// What proves a connecting peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthBackend {
    /// No peer auth at all (loopback UDS, or a deliberately-open public bind).
    None,
    /// First-connection-pins mTLS.
    Tofu,
    /// `tailscale whois` resolved.
    Tailscale,
}

/// What triggered the posture event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PostureSource {
    /// `SettingsResolver` set/unset of a tracked security key.
    ConfigChange,
    /// Listener bind/unbind.
    ListenerBind,
    /// Daemon-startup reconciliation.
    DaemonStartReconcile,
}

/// Per-`xp_gain` breakdown stored in `evolution_events.metadata_json` so
/// `borg audit verify` can re-derive `final_xp` from the chain alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct XpBreakdown {
    /// `0`, `1`, or `2` from the action table.
    pub base: u32,
    /// `0` or `1` archetype-alignment bonus.
    pub align_bonus: u32,
    /// `0.85` / `1.00` / `1.10`.
    pub state_multiplier: f32,
    pub state: PostureState,
    /// `(base + align_bonus) * state_multiplier`, pre-clamp.
    pub pre_clamp: f32,
    /// `round_half_up(pre_clamp)` clamped to the per-event XP floor/ceiling.
    pub clamped: u32,
    /// Final XP written to `evolution_events.xp_delta`.
    pub final_xp: u32,
    /// `evolution_events.id` of the governing `security_posture_changed`
    /// row, or `0` when written under [`FallbackReason::PostureUndetermined`].
    pub posture_event_seq: i64,
    /// Set when posture was unavailable at write time. Surfaces in
    /// `borg doctor` after two consecutive sweeps see fallback events.
    pub fallback: Option<FallbackReason>,
}

/// Why a write fell back to `Default × 1.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FallbackReason {
    /// No posture event in the chain yet (early-startup write).
    PostureUndetermined,
    /// Replay rejected the most recent `security_posture_changed` row.
    PostureChainBroken,
    /// `PostureSnapshot.schema_version` is newer than this binary.
    SchemaVersionUnknown,
}

/// Compute the final XP for a single `xp_gain` write.
///
/// Math:
///
/// 1. `with_alignment = base + align_bonus`               (capped at MAX by callers)
/// 2. `with_security  = with_alignment × multiplier`
/// 3. `clamped        = clamp(round_half_up(with_security), floor, MAX_XP_DELTA)`
///    where `floor = 1` if `with_alignment > 0` else `0`.
///
/// Returns the populated [`XpBreakdown`] so callers can persist it verbatim.
pub fn compute_xp_breakdown(
    base: u32,
    align_bonus: u32,
    state: PostureState,
    posture_event_seq: i64,
    fallback: Option<FallbackReason>,
) -> XpBreakdown {
    let with_alignment = base.saturating_add(align_bonus);
    let multiplier = state.xp_multiplier();
    let pre_clamp = with_alignment as f32 * multiplier;
    let floor = if with_alignment > 0 { 1u32 } else { 0u32 };
    let rounded = round_half_up(pre_clamp);
    let clamped = rounded.clamp(floor, MAX_XP_DELTA);
    XpBreakdown {
        base,
        align_bonus,
        state_multiplier: multiplier,
        state,
        pre_clamp,
        clamped,
        final_xp: clamped,
        posture_event_seq,
        fallback,
    }
}

/// `round_half_up` for non-negative `f32` → `u32`. We do *not* use Rust's
/// `f32::round` directly because its "round half away from zero" behavior
/// matches our needs only for non-negative inputs — but XP values are
/// always non-negative so the spec calls for `round_half_up` explicitly.
fn round_half_up(x: f32) -> u32 {
    if x < 0.0 {
        // Spec: XP can never be negative. Saturating here preserves that.
        return 0;
    }
    (x + 0.5).floor() as u32
}

#[cfg(test)]
mod multiplier_tests {
    use super::*;

    /// Boosted aligned creation must clamp at 3, not overflow to 3.3 → 3.
    /// Regression target: dropping the `.clamp(.., MAX_XP_DELTA)` would let
    /// future per-event-cap raises silently leak through.
    #[test]
    fn boosted_aligned_creation_caps_at_three() {
        let b = compute_xp_breakdown(2, 1, PostureState::Boosted, 42, None);
        assert!((b.pre_clamp - 3.3).abs() < 1e-5);
        assert_eq!(b.final_xp, MAX_XP_DELTA);
        assert_eq!(b.posture_event_seq, 42);
    }

    /// Reduced single tool-call must honor the floor — 0.85 rounds to 1, not 0.
    /// The `else 0` branch must only fire for `with_alignment == 0`.
    #[test]
    fn reduced_single_tool_call_honors_floor_of_one() {
        let b = compute_xp_breakdown(1, 0, PostureState::Reduced, 1, None);
        assert!((b.pre_clamp - 0.85).abs() < 1e-6);
        assert_eq!(b.final_xp, 1);
    }

    /// Zero-input action stays 0 even under Boosted (floor is conditional).
    #[test]
    fn zero_input_stays_zero_under_boosted() {
        let b = compute_xp_breakdown(0, 0, PostureState::Boosted, 1, None);
        assert_eq!(b.pre_clamp, 0.0);
        assert_eq!(b.final_xp, 0);
    }

    /// Default × everything is identity within the cap.
    #[test]
    fn default_is_identity() {
        for (base, align) in [(0, 0), (1, 0), (1, 1), (2, 0), (2, 1)] {
            let b = compute_xp_breakdown(base, align, PostureState::Default, 1, None);
            let expected = (base + align).min(MAX_XP_DELTA);
            assert_eq!(
                b.final_xp, expected,
                "default(base={base}, align={align}) → {expected}"
            );
        }
    }

    /// Round-half-up: 1 × 0.85 = 0.85 → 1 (not 0). 2 × 0.85 = 1.70 → 2.
    /// 1 × 1.10 = 1.10 → 1. 2 × 1.10 = 2.20 → 2. 3 × 1.10 = 3.30 → cap to 3.
    #[test]
    fn round_half_up_matrix() {
        let cases = [
            (1, PostureState::Reduced, 1u32),
            (2, PostureState::Reduced, 2u32),
            (1, PostureState::Boosted, 1u32),
            (2, PostureState::Boosted, 2u32),
            (3, PostureState::Boosted, 3u32),
        ];
        for (with_alignment, state, expected) in cases {
            // Express as base alone so align_bonus stays at 0; the math
            // doesn't care which input contributes.
            let b = compute_xp_breakdown(with_alignment, 0, state, 1, None);
            assert_eq!(
                b.final_xp, expected,
                "with_alignment={with_alignment} state={state:?} → {expected}"
            );
        }
    }

    /// Fallback is plumbed verbatim — used by doctor sweep escalation.
    #[test]
    fn fallback_round_trips_through_breakdown() {
        let b = compute_xp_breakdown(
            1,
            0,
            PostureState::Default,
            0,
            Some(FallbackReason::PostureUndetermined),
        );
        assert_eq!(b.fallback, Some(FallbackReason::PostureUndetermined));
        assert_eq!(b.posture_event_seq, 0);
    }

    /// `PostureState::from_str` round-trip — chain-replay uses it to parse
    /// back what `as_str` wrote, so a typo on either side breaks audit
    /// verifiability.
    #[test]
    fn posture_state_str_roundtrip() {
        for s in [
            PostureState::Reduced,
            PostureState::Default,
            PostureState::Boosted,
        ] {
            assert_eq!(PostureState::parse(s.as_str()), Some(s));
        }
        assert_eq!(PostureState::parse("Hardened"), None);
    }
}
