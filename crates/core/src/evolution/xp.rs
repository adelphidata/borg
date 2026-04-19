//! XP curve and level derivation.
//!
//! Pure functions that map `(Stage, XP)` → `level` and vice versa. The
//! curve is intentionally stage-dependent — base stage is fast (linear),
//! final stage is exponential — and level caps at 99.

use super::Stage;

/// XP required for a specific level at a given stage.
/// WoW-style: Stage 1 is fast (linear), Stage 2 moderate, Stage 3 exponential.
pub fn xp_for_level(stage: &Stage, level: u8) -> u32 {
    let n = level as f64;
    match stage {
        Stage::Base => 20 + (n.powf(1.4)) as u32, // base=20, curve=1.4
        Stage::Evolved => 40 + (n.powf(1.55)) as u32, // base=40, curve=1.55
        Stage::Final => 80 + (n.powf(1.8)) as u32, // base=80, curve=1.8
    }
}

/// Total XP required to reach a given level from Lvl.0.
pub fn total_xp_for_level(stage: &Stage, target_level: u8) -> u32 {
    (0..target_level).map(|n| xp_for_level(stage, n)).sum()
}

/// Given accumulated XP in current stage, compute (level, xp_remaining_to_next).
///
/// Level is capped at 99. Beyond Lvl.99 we report `(99, 0)` regardless of
/// additional XP accumulated — the curve is not defined past this ceiling
/// and the `for` bound below must stay at `0..99` to keep the saturating
/// behavior.
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
    (99, 0)
}
