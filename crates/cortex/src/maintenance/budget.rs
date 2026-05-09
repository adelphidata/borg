//! Heal-action budget tracker. Caps the total number of mutating self-heal
//! actions one maintenance sweep is allowed to perform — the backstop
//! against runaway repair loops.

/// Caps the total number of mutating self-heal actions one sweep is
/// allowed to perform. Once `spent >= budget`, every subsequent
/// `try_spend` returns `false` and the sweep skips its remaining
/// mutating steps. Pure data — testable without a real DB.
#[derive(Debug)]
pub(super) struct BudgetTracker {
    pub(super) budget: u32,
    pub(super) spent: u32,
    pub(super) blown: bool,
}

impl BudgetTracker {
    pub(super) fn new(budget: u32) -> Self {
        Self {
            budget,
            spent: 0,
            blown: false,
        }
    }

    /// Charge `n` actions to the budget. Returns `true` while there is
    /// still room (i.e. the caller may proceed); returns `false` once
    /// the budget is blown — and stays `false` for the rest of the
    /// sweep. `saturating_add` guards against a step reporting a wildly
    /// wrong count.
    pub(super) fn try_spend(&mut self, n: usize) -> bool {
        if self.blown {
            return false;
        }
        let charge = n.min(u32::MAX as usize) as u32;
        self.spent = self.spent.saturating_add(charge);
        if self.spent >= self.budget {
            self.blown = true;
        }
        !self.blown
    }

    /// Read-only view: is the budget blown? (Independent of any pending
    /// `try_spend` call — used by remediation loops to short-circuit
    /// before doing more work.)
    pub(super) fn is_blown(&self) -> bool {
        self.blown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_tracker_blocks_after_exhaust_and_stays_blocked() {
        let mut t = BudgetTracker::new(10);
        assert!(t.try_spend(3));
        assert!(t.try_spend(3));
        // 6 spent; one more 3 puts us at 9 (still healthy).
        assert!(t.try_spend(3));
        // Now at 9. Spend 1 → 10 → blown.
        assert!(!t.try_spend(1));
        // Once blown, every subsequent attempt is a no-op: returns false,
        // does NOT keep accumulating spent. The final `spent` value is
        // the count at the moment the budget tipped — that's the number
        // we report so users can tune the cap accurately.
        assert!(!t.try_spend(50));
        assert_eq!(
            t.spent, 10,
            "spent must freeze at the tipping value, not keep climbing"
        );
        assert!(t.is_blown());
    }

    #[test]
    fn budget_tracker_saturates_on_giant_charge() {
        let mut t = BudgetTracker::new(100);
        // n past u32::MAX must not panic.
        assert!(!t.try_spend(usize::MAX));
        assert!(t.is_blown());
    }
}
