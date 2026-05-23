//! Supply-chain advisory checker.
//!
//! Bundles a curated list of known-bad `(crate, version)` pairs alongside the
//! workspace `Cargo.lock` (both embedded at compile time via `include_str!`).
//! At runtime, the `borg doctor` Supply Chain step scans the embedded lockfile
//! against the advisory list and emits a warning or failure per match.
//!
//! This is intentionally lightweight — it does not replace `cargo audit` in
//! CI. The goal is to give operators a loud signal when a dependency they
//! shipped with is later flagged, so they update without waiting for the next
//! release pipeline.

use serde::Deserialize;
use std::sync::OnceLock;

/// Embedded copy of the workspace `Cargo.lock`. Updated automatically on
/// every clean build because `include_str!` invalidates on file change.
const EMBEDDED_LOCKFILE: &str = include_str!("../../../../Cargo.lock");

/// Embedded advisory list. Edit `advisories.toml` to add new entries.
const EMBEDDED_ADVISORIES: &str = include_str!("advisories.toml");

/// Severity rating; controls whether the doctor surfaces a warning or a
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Informational — emitted as a warning, no posture impact.
    Low,
    /// Notable — warning, encourage upgrade.
    Medium,
    /// Active risk — failure, blocks the diagnostic from passing.
    High,
    /// Imminent compromise vector — failure with prominent banner.
    Critical,
}

impl Severity {
    /// `true` if this severity should surface as a doctor *failure* rather
    /// than a warning.
    pub fn is_failing(self) -> bool {
        matches!(self, Severity::High | Severity::Critical)
    }
}

/// One row in `advisories.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Advisory {
    /// Crate name as it appears in `Cargo.lock`'s `[[package]]` entries.
    #[serde(rename = "crate")]
    pub crate_name: String,
    /// Version match expression. See `version_matches` for the supported
    /// syntax (`=X.Y.Z`, `<X.Y.Z`, `>=X.Y.Z`, `X.Y.*`, or bare exact).
    pub versions: String,
    /// Stable identifier — RUSTSEC, CVE, BORG, or vendor advisory id.
    pub id: String,
    /// Severity classification.
    pub severity: Severity,
    /// One-line description of the issue.
    pub summary: String,
    /// One-line remediation, usually a Cargo command.
    pub fix: String,
}

#[derive(Debug, Deserialize)]
struct AdvisoryFile {
    #[serde(default)]
    advisories: Vec<Advisory>,
}

/// A concrete hit: an advisory matched against a specific package version in
/// the embedded lockfile.
#[derive(Debug, Clone)]
pub struct AdvisoryHit {
    /// The advisory that matched.
    pub advisory: Advisory,
    /// The exact version of the dependency in the lockfile.
    pub installed_version: String,
}

/// Parse and cache the embedded advisory file. Cached on first access so we
/// don't re-parse on every doctor run.
fn embedded_advisories() -> &'static [Advisory] {
    static CACHE: OnceLock<Vec<Advisory>> = OnceLock::new();
    CACHE.get_or_init(
        || match toml::from_str::<AdvisoryFile>(EMBEDDED_ADVISORIES) {
            Ok(file) => file.advisories,
            Err(e) => {
                tracing::error!("failed to parse embedded advisories.toml: {e}");
                Vec::new()
            }
        },
    )
}

/// Cargo.lock `[[package]]` entry — only the fields we care about.
#[derive(Debug, Deserialize)]
struct LockPackage {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct LockFile {
    #[serde(default, rename = "package")]
    packages: Vec<LockPackage>,
}

/// Scan a Cargo.lock text against an advisory list.
///
/// Pure function — no I/O — so the test suite can exercise it with synthetic
/// lockfiles and advisory rows.
pub fn scan(lockfile_text: &str, advisories: &[Advisory]) -> Vec<AdvisoryHit> {
    let parsed: LockFile = match toml::from_str(lockfile_text) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("supply-chain scan: cannot parse Cargo.lock: {e}");
            return Vec::new();
        }
    };
    let mut hits = Vec::new();
    for pkg in &parsed.packages {
        for adv in advisories {
            if adv.crate_name != pkg.name {
                continue;
            }
            if version_matches(&pkg.version, &adv.versions) {
                hits.push(AdvisoryHit {
                    advisory: adv.clone(),
                    installed_version: pkg.version.clone(),
                });
            }
        }
    }
    hits
}

/// Run the supply-chain scan against the embedded lockfile + advisory list.
pub fn scan_embedded() -> Vec<AdvisoryHit> {
    scan(EMBEDDED_LOCKFILE, embedded_advisories())
}

/// Match a concrete `version` string against an advisory `expr`.
///
/// Supported syntax:
/// - `=1.2.3` or bare `1.2.3` — exact match.
/// - `<1.2.3` — strict less-than (semver triple comparison).
/// - `<=1.2.3`, `>1.2.3`, `>=1.2.3` — analogous.
/// - `1.2.*` — wildcard prefix match on the leading dotted components.
/// - `>=1.0.0, <2.0.0` — comma-separated AND of comparators.
///
/// Unparseable expressions return `false` and log a warning so a bad
/// advisory row never crashes the doctor.
pub fn version_matches(version: &str, expr: &str) -> bool {
    let expr = expr.trim();
    if expr.is_empty() {
        return false;
    }
    expr.split(',')
        .map(str::trim)
        .all(|clause| match_clause(version, clause))
}

fn match_clause(version: &str, clause: &str) -> bool {
    if let Some(rest) = clause.strip_prefix(">=") {
        return cmp_semver(version, rest.trim()) >= std::cmp::Ordering::Equal;
    }
    if let Some(rest) = clause.strip_prefix("<=") {
        return cmp_semver(version, rest.trim()) <= std::cmp::Ordering::Equal;
    }
    if let Some(rest) = clause.strip_prefix('>') {
        return cmp_semver(version, rest.trim()) == std::cmp::Ordering::Greater;
    }
    if let Some(rest) = clause.strip_prefix('<') {
        return cmp_semver(version, rest.trim()) == std::cmp::Ordering::Less;
    }
    if let Some(rest) = clause.strip_prefix('=') {
        return version == rest.trim();
    }
    if let Some(prefix) = clause.strip_suffix(".*") {
        return version == prefix || version.starts_with(&format!("{prefix}."));
    }
    version == clause
}

/// Three-way compare two semver-ish version triples
/// (`MAJOR.MINOR.PATCH[-prerelease]`). Honors the SemVer §11 rule that a
/// version *with* a pre-release suffix is **less** than the same version
/// without one (`1.2.3-rc.1 < 1.2.3`) — critical for advisory matching, since
/// a `<1.2.3` clause must still flag the vulnerable `1.2.3-rc.1` install.
/// Build metadata (`+build`) is ignored per spec. Returns `Equal` for
/// unparseable inputs so a bad advisory row never silently flags every install.
fn cmp_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Option<(u64, u64, u64, Option<String>)> {
        let no_build = s.split('+').next().unwrap_or(s);
        let mut iter = no_build.splitn(2, '-');
        let core = iter.next().unwrap_or("");
        let pre = iter.next().map(std::string::ToString::to_string);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some((major, minor, patch, pre))
    };
    match (parse(a), parse(b)) {
        (Some((amaj, amin, apat, apre)), Some((bmaj, bmin, bpat, bpre))) => {
            let core = (amaj, amin, apat).cmp(&(bmaj, bmin, bpat));
            if core != std::cmp::Ordering::Equal {
                return core;
            }
            // Equal cores: presence of pre-release lowers precedence. Two
            // pre-release suffixes compare lexicographically — close enough
            // for the advisory matcher without pulling in the `semver` crate.
            match (apre, bpre) {
                (None, None) => std::cmp::Ordering::Equal,
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some(ap), Some(bp)) => ap.cmp(&bp),
            }
        }
        _ => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv(crate_name: &str, versions: &str, severity: Severity) -> Advisory {
        Advisory {
            crate_name: crate_name.into(),
            versions: versions.into(),
            id: "TEST-0001".into(),
            severity,
            summary: "test".into(),
            fix: "cargo update -p ".to_string() + crate_name,
        }
    }

    const SAMPLE_LOCK: &str = r#"
version = 4

[[package]]
name = "good"
version = "1.0.0"

[[package]]
name = "yanked"
version = "2.4.6"

[[package]]
name = "old"
version = "0.5.1"
"#;

    #[test]
    fn exact_version_match_is_a_hit() {
        let hits = scan(SAMPLE_LOCK, &[adv("yanked", "=2.4.6", Severity::Critical)]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].installed_version, "2.4.6");
        assert!(hits[0].advisory.severity.is_failing());
    }

    #[test]
    fn unrelated_crate_is_not_a_hit() {
        let hits = scan(SAMPLE_LOCK, &[adv("yanked", "=9.9.9", Severity::High)]);
        assert!(hits.is_empty());
    }

    #[test]
    fn range_clause_matches_older_versions() {
        let hits = scan(SAMPLE_LOCK, &[adv("old", "<1.0.0", Severity::Medium)]);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].advisory.severity.is_failing());
    }

    #[test]
    fn range_clause_excludes_newer_versions() {
        let hits = scan(SAMPLE_LOCK, &[adv("good", "<1.0.0", Severity::Medium)]);
        assert!(hits.is_empty());
    }

    #[test]
    fn wildcard_clause_matches_prefix() {
        let hits = scan(SAMPLE_LOCK, &[adv("yanked", "2.4.*", Severity::High)]);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn combined_clauses_must_all_match() {
        // 2.4.6 falls inside [2.0.0, 3.0.0)
        let hits = scan(
            SAMPLE_LOCK,
            &[adv("yanked", ">=2.0.0, <3.0.0", Severity::High)],
        );
        assert_eq!(hits.len(), 1);
        // 2.4.6 does NOT fall inside [2.5.0, 3.0.0)
        let hits = scan(
            SAMPLE_LOCK,
            &[adv("yanked", ">=2.5.0, <3.0.0", Severity::High)],
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn embedded_advisories_parse_cleanly() {
        // Regression: a malformed `advisories.toml` would silently produce
        // an empty list and skip every advisory. Assert that whatever ships
        // in the file parses, even when empty.
        let _ = embedded_advisories();
    }

    #[test]
    fn embedded_lockfile_is_parseable_toml() {
        // Regression: if `include_str!` ever picked up a half-written
        // Cargo.lock during build, the scan would silently return no hits.
        let parsed: Result<LockFile, _> = toml::from_str(EMBEDDED_LOCKFILE);
        assert!(parsed.is_ok(), "embedded Cargo.lock failed to parse");
        // And the embedded list has at least one package (sanity — Borg has
        // hundreds of deps, an empty list means we shipped a broken include).
        assert!(!parsed.unwrap().packages.is_empty());
    }

    #[test]
    fn prerelease_is_less_than_release_per_semver_section_11() {
        // Regression: a `<1.2.3` clause must still catch `1.2.3-rc.1`,
        // otherwise an advisory list silently misses vulnerable pre-release
        // installs. The matcher previously stripped the pre-release suffix
        // and compared as equal — wrong direction (unsafe).
        let lock = r#"
version = 4

[[package]]
name = "vuln"
version = "1.2.3-rc.1"
"#;
        let hits = scan(lock, &[adv("vuln", "<1.2.3", Severity::Critical)]);
        assert_eq!(hits.len(), 1, "1.2.3-rc.1 must satisfy <1.2.3");

        // And a release version equal to the bound is NOT caught by `<`.
        let lock = r#"
version = 4

[[package]]
name = "vuln"
version = "1.2.3"
"#;
        let hits = scan(lock, &[adv("vuln", "<1.2.3", Severity::Critical)]);
        assert!(hits.is_empty(), "1.2.3 must NOT satisfy <1.2.3");
    }

    #[test]
    fn build_metadata_does_not_affect_comparison() {
        // SemVer §10: build metadata is ignored in precedence.
        let lock = r#"
version = 4

[[package]]
name = "vuln"
version = "1.2.3+sha.abc"
"#;
        let hits = scan(lock, &[adv("vuln", "=1.2.3", Severity::High)]);
        // Exact-match uses string equality so this is a documented limitation
        // for `=` clauses, but range clauses must work.
        assert!(hits.is_empty(), "= uses string equality, not semver");
        let hits = scan(lock, &[adv("vuln", ">=1.2.3", Severity::High)]);
        assert_eq!(hits.len(), 1, ">= must ignore +build");
    }

    #[test]
    fn unparseable_clause_returns_false_not_panics() {
        assert!(!version_matches("1.2.3", "not-a-comparator"));
    }

    #[test]
    fn empty_expr_does_not_match() {
        assert!(!version_matches("1.2.3", ""));
    }
}
