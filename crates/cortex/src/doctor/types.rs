//! Diagnostic types: `RemediationKind`, `CheckStatus`, `DiagnosticCheck`,
//! `DiagnosticReport`.
//!
//! Extracted from `doctor.rs` so the type machinery isn't interleaved with
//! the 21 individual check functions. The runner (`DiagnosticRunner`) stays
//! in `doctor.rs` because its step-by-step iteration is tightly coupled to
//! the concrete check dispatch.

use anyhow::Result;

/// How a detected issue can be remediated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemediationKind {
    /// No automated or guided remediation metadata.
    None,
    /// Safe to repair automatically during `/heal` / maintenance.
    AutoSafe,
    /// Potentially automatable later, but intentionally not in the current sweep.
    AutoGuarded,
    /// Requires explicit manual user action.
    Manual,
}

/// Result status for a single diagnostic check.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckStatus {
    /// Check passed successfully.
    Pass,
    /// Check passed with a warning.
    Warn(String),
    /// Check failed.
    Fail(String),
}

/// A single diagnostic check result with category, name, and status.
#[derive(Debug, Clone)]
pub struct DiagnosticCheck {
    pub category: &'static str,
    pub name: String,
    pub status: CheckStatus,
    pub remediation_kind: RemediationKind,
    pub remediation_key: Option<&'static str>,
    pub user_hint: Option<String>,
}

impl DiagnosticCheck {
    /// Create a passing check.
    pub fn pass(category: &'static str, name: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Pass,
            remediation_kind: RemediationKind::None,
            remediation_key: None,
            user_hint: None,
        }
    }

    /// Create a warning check.
    pub fn warn(category: &'static str, name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Warn(msg.into()),
            remediation_kind: RemediationKind::None,
            remediation_key: None,
            user_hint: None,
        }
    }

    /// Create a failing check.
    pub fn fail(category: &'static str, name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Fail(msg.into()),
            remediation_kind: RemediationKind::None,
            remediation_key: None,
            user_hint: None,
        }
    }

    /// Create a check with an explicit status.
    pub fn with_status(
        category: &'static str,
        name: impl Into<String>,
        status: CheckStatus,
    ) -> Self {
        Self {
            category,
            name: name.into(),
            status,
            remediation_kind: RemediationKind::None,
            remediation_key: None,
            user_hint: None,
        }
    }

    /// Attach remediation metadata to a check.
    pub fn with_remediation(
        mut self,
        remediation_kind: RemediationKind,
        remediation_key: &'static str,
        user_hint: impl Into<String>,
    ) -> Self {
        self.remediation_kind = remediation_kind;
        self.remediation_key = Some(remediation_key);
        self.user_hint = Some(user_hint.into());
        self
    }

    /// Stable issue key persisted by maintenance.
    pub fn issue_key(&self) -> String {
        format!("{}:{}", self.category, self.name)
    }

    /// Create a check from a Result: pass on Ok, fail on Err.
    pub fn from_result<T, E: std::fmt::Display>(
        category: &'static str,
        name: impl Into<String>,
        result: Result<T, E>,
    ) -> Self {
        match result {
            Ok(_) => Self::pass(category, name),
            Err(e) => Self::fail(category, name, format!("{e}")),
        }
    }

    /// Format a single check result line with status icon.
    pub fn format_line(&self) -> String {
        let suffix = match self.remediation_kind {
            RemediationKind::AutoSafe => " [auto-fixable]",
            RemediationKind::AutoGuarded => " [guarded]",
            RemediationKind::Manual => " [manual]",
            RemediationKind::None => "",
        };
        match &self.status {
            CheckStatus::Pass => format!("  ✓ {}", self.name),
            CheckStatus::Warn(msg) => format!("  ⚠ {}{suffix} — {msg}", self.name),
            CheckStatus::Fail(msg) => format!("  ✗ {}{suffix} — {msg}", self.name),
        }
    }
}

/// Collection of diagnostic checks grouped by category.
pub struct DiagnosticReport {
    pub checks: Vec<DiagnosticCheck>,
}

impl DiagnosticReport {
    /// Format the report as a human-readable string with status icons.
    pub fn format(&self) -> String {
        let mut output = String::from("Borg Doctor\n───────────\n");
        let mut current_category = "";

        for check in &self.checks {
            if check.category != current_category {
                current_category = check.category;
                output.push_str(&format!("\n{current_category}\n"));
            }
            output.push_str(&check.format_line());
            output.push('\n');
        }

        let (pass_count, warn_count, fail_count) = self.counts();
        output.push_str(&format!(
            "\nSummary: {pass_count} passed, {warn_count} warning(s), {fail_count} failed"
        ));
        output
    }

    /// Count (pass, warn, fail) results.
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut warn = 0;
        let mut fail = 0;
        for check in &self.checks {
            match &check.status {
                CheckStatus::Pass => pass += 1,
                CheckStatus::Warn(_) => warn += 1,
                CheckStatus::Fail(_) => fail += 1,
            }
        }
        (pass, warn, fail)
    }
}
