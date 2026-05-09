//! Diagnostic issue snapshots and remediation grouping.
//!
//! Helper types and functions used by `run_daily_maintenance` to track
//! which doctor checks failed, which were auto-fixed, and which still
//! need attention.

use std::collections::{BTreeMap, BTreeSet};

use crate::doctor::{CheckStatus, DiagnosticCheck, RemediationKind};

#[derive(Debug, Clone)]
pub(super) struct IssueSnapshot {
    pub(super) issue_key: String,
    pub(super) status: &'static str,
    pub(super) remediation_kind: RemediationKind,
    pub(super) remediation_key: Option<&'static str>,
}

#[derive(Debug, Default)]
pub(super) struct RepairExecution {
    pub(super) fixed_issues: BTreeSet<String>,
    pub(super) failed_issues: BTreeSet<String>,
}

pub(super) fn issue_snapshots(checks: &[DiagnosticCheck]) -> Vec<IssueSnapshot> {
    checks
        .iter()
        .filter_map(|check| match &check.status {
            CheckStatus::Pass => None,
            CheckStatus::Warn(_) => Some(IssueSnapshot {
                issue_key: check.issue_key(),
                status: "warn",
                remediation_kind: check.remediation_kind,
                remediation_key: check.remediation_key,
            }),
            CheckStatus::Fail(_) => Some(IssueSnapshot {
                issue_key: check.issue_key(),
                status: "fail",
                remediation_kind: check.remediation_kind,
                remediation_key: check.remediation_key,
            }),
        })
        .collect()
}

pub(super) fn group_issues_by_remediation(
    issues: &[IssueSnapshot],
) -> BTreeMap<&'static str, Vec<&IssueSnapshot>> {
    let mut out: BTreeMap<&'static str, Vec<&IssueSnapshot>> = BTreeMap::new();
    for issue in issues {
        if issue.remediation_kind != RemediationKind::AutoSafe {
            continue;
        }
        if let Some(key) = issue.remediation_key {
            out.entry(key).or_default().push(issue);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_issues_by_remediation_deduplicates_auto_safe_keys() {
        let issues = vec![
            IssueSnapshot {
                issue_key: "Embeddings:chunk coverage".into(),
                status: "warn",
                remediation_kind: RemediationKind::AutoSafe,
                remediation_key: Some("embeddings.repair"),
            },
            IssueSnapshot {
                issue_key: "Embeddings:whole-entry coverage".into(),
                status: "warn",
                remediation_kind: RemediationKind::AutoSafe,
                remediation_key: Some("embeddings.repair"),
            },
            IssueSnapshot {
                issue_key: "Gateway:channel manifest".into(),
                status: "fail",
                remediation_kind: RemediationKind::Manual,
                remediation_key: None,
            },
        ];
        let grouped = group_issues_by_remediation(&issues);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped["embeddings.repair"].len(), 2);
    }
}
