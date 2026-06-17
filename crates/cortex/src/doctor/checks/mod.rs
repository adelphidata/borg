//! Individual diagnostic check groups.
//!
//! Each submodule owns a cohesive family of checks plus its tests. The
//! [`super::StepKind`] dispatcher calls into the `check_*` entry points here;
//! everything else (classifiers, coverage probes) stays private to its module.

pub(super) mod memory;
pub(super) mod provider;
