//! Individual diagnostic check groups.
//!
//! Each submodule owns a cohesive family of checks plus its tests. The
//! [`super::StepKind`] dispatcher calls into the `check_*` entry points here;
//! everything else (classifiers, coverage probes) stays private to its module.

pub(super) mod data;
pub(super) mod memory;
pub(super) mod network;
pub(super) mod provider;
pub(super) mod runtime;
pub(super) mod security;
pub(super) mod system;

/// Status predicates shared by the per-module check tests.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::doctor::{CheckStatus, DiagnosticCheck};

    pub(crate) fn fails(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Fail(_))
    }

    pub(crate) fn warns(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Warn(_))
    }

    pub(crate) fn passes(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Pass)
    }
}
