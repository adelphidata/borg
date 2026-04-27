//! XP curve and level derivation.
//!
//! Pure functions that map `(Stage, XP)` → `level` and vice versa. Stage 1
//! (Base) is fast (linear-ish), Stage 2 (Evolved) moderate, Stage 3 (Final)
//! exponential up to Lvl.99 then linear post-99.
//!
//! Base/Evolved cap at Lvl.99. Final has no cap — past Lvl.99 the curve is
//! piecewise: each level costs a flat 50 XP, giving long-term users an
//! ongoing reason to engage.

use super::Stage;

/// Flat XP cost per level past Lvl.99 in Final stage.
pub(crate) const FINAL_POST_99_LEVEL_COST: u32 = 50;

/// XP required for a specific level at a given stage.
///
/// For Final stage, the curve is piecewise:
/// - `level ≤ 99`: `80 + floor(level^1.8)` (existing exponential curve)
/// - `level > 99`: flat `FINAL_POST_99_LEVEL_COST` (50) per level
pub fn xp_for_level(stage: &Stage, level: u8) -> u32 {
    let n = level as f64;
    match stage {
        Stage::Base => 20 + (n.powf(1.4)) as u32, // base=20, curve=1.4
        Stage::Evolved => 40 + (n.powf(1.55)) as u32, // base=40, curve=1.55
        Stage::Final => {
            if level <= 99 {
                80 + (n.powf(1.8)) as u32
            } else {
                FINAL_POST_99_LEVEL_COST
            }
        }
    }
}

/// Total XP required to reach a given level from Lvl.0.
pub fn total_xp_for_level(stage: &Stage, target_level: u8) -> u32 {
    (0..target_level).map(|n| xp_for_level(stage, n)).sum()
}

/// Given accumulated XP in current stage, compute (level, xp_remaining_to_next).
///
/// Base and Evolved cap at Lvl.99 — additional XP past that point reports
/// `(99, 0)`. Final has no cap and continues to climb at
/// `FINAL_POST_99_LEVEL_COST` (50) XP per level past 99.
pub fn level_from_xp(stage: &Stage, xp: u32) -> (u8, u32) {
    let mut remaining = xp;
    for lvl in 0..99u8 {
        let cost = xp_for_level(stage, lvl);
        debug_assert!(cost > 0, "xp_for_level must be positive for level < 99");
        if remaining < cost {
            return (lvl, cost - remaining);
        }
        remaining -= cost;
    }
    // At Lvl.99: Base/Evolved saturate, Final continues piecewise.
    match stage {
        Stage::Base | Stage::Evolved => (99, 0),
        Stage::Final => {
            // Each level past 99 costs FINAL_POST_99_LEVEL_COST.
            let extra_levels = remaining / FINAL_POST_99_LEVEL_COST;
            let level = 99u32.saturating_add(extra_levels).min(u8::MAX as u32) as u8;
            let consumed = extra_levels * FINAL_POST_99_LEVEL_COST;
            let to_next = FINAL_POST_99_LEVEL_COST - (remaining - consumed);
            (level, to_next)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_caps_at_99() {
        let (lvl, _) = level_from_xp(&Stage::Base, u32::MAX);
        assert_eq!(lvl, 99);
    }

    #[test]
    fn evolved_caps_at_99() {
        let (lvl, _) = level_from_xp(&Stage::Evolved, u32::MAX);
        assert_eq!(lvl, 99);
    }

    #[test]
    fn final_does_not_cap_at_99() {
        // Cumulative XP for Lvl.99 in Final + 50 XP buys exactly Lvl.100.
        let to_99 = total_xp_for_level(&Stage::Final, 99);
        let (lvl, to_next) = level_from_xp(&Stage::Final, to_99 + FINAL_POST_99_LEVEL_COST);
        assert_eq!(lvl, 100, "expected Lvl.100 after exact post-99 increment");
        assert_eq!(to_next, FINAL_POST_99_LEVEL_COST);
    }

    #[test]
    fn final_post_99_costs_50_per_level() {
        let to_99 = total_xp_for_level(&Stage::Final, 99);
        // 5 full extra levels worth of XP.
        let xp = to_99 + 5 * FINAL_POST_99_LEVEL_COST;
        let (lvl, to_next) = level_from_xp(&Stage::Final, xp);
        assert_eq!(lvl, 104);
        assert_eq!(to_next, FINAL_POST_99_LEVEL_COST);
    }

    #[test]
    fn final_post_99_partial_progress() {
        let to_99 = total_xp_for_level(&Stage::Final, 99);
        // Halfway through Lvl.100 → still Lvl.99, 25 XP to next.
        let (lvl, to_next) = level_from_xp(&Stage::Final, to_99 + 25);
        assert_eq!(lvl, 99);
        assert_eq!(to_next, 25);
    }

    #[test]
    fn xp_for_level_99_to_100_transition_in_final() {
        // The piecewise transition is intentionally not C¹-smooth — Lvl.99
        // costs 80 + floor(99^1.8) ≈ a few thousand, while Lvl.100+ costs a
        // flat 50. This test pins the boundary so accidental refactors that
        // smooth the curve fail loudly.
        let cost_99 = xp_for_level(&Stage::Final, 99);
        let cost_100 = xp_for_level(&Stage::Final, 100);
        assert!(
            cost_99 > cost_100,
            "Lvl.99 cost ({cost_99}) should exceed flat post-99 cost ({cost_100})",
        );
        assert_eq!(cost_100, FINAL_POST_99_LEVEL_COST);
        assert_eq!(xp_for_level(&Stage::Final, 200), FINAL_POST_99_LEVEL_COST);
    }
}
