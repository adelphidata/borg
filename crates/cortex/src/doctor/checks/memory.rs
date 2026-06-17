//! Memory and embedding-index health checks.

use crate::config::Config;
use crate::constants::IDENTITY_FILE;
use crate::db::Database;
use crate::doctor::{DiagnosticCheck, RemediationKind};

/// Max acceptable gap between memory entries and indexed embeddings before
/// the doctor escalates to a warning. Below this threshold the gap is the
/// normal write/embed race window — the next embed pass closes it.
const EMBEDDING_LAG_WARN_THRESHOLD: usize = 5;

async fn check_memory_provider(
    config: &Config,
    checks: &mut Vec<DiagnosticCheck>,
    probe_network: bool,
) {
    use crate::config::media::MemoryProviderKind;

    let selected = config.memory.provider;
    if matches!(selected, MemoryProviderKind::Native) {
        checks.push(DiagnosticCheck::pass("Memory", "provider: native (SQLite)"));
        return;
    }

    let prov = crate::memory::provider::from_config(config);
    // The registry returns native when an external provider is selected
    // but its credentials are missing/unresolvable.
    if prov.name() == "native" {
        checks.push(
            DiagnosticCheck::warn(
                "Memory",
                "provider",
                format!("memory.provider={selected} is unconfigured — falling back to native"),
            )
            .with_remediation(
                RemediationKind::Manual,
                "memory.provider_unconfigured",
                format!("set credentials, e.g. `borg settings set memory.{selected}.api_key`"),
            ),
        );
        return;
    }

    // Configured external provider. Without a network probe (unattended
    // path) just confirm it's selected + configured; the interactive path
    // additionally verifies reachability.
    if !probe_network {
        checks.push(DiagnosticCheck::pass(
            "Memory",
            format!("provider '{}' configured", prov.name()),
        ));
        return;
    }

    let ctx = crate::memory::provider::MemCtx::from_config(config);
    match prov.health(&ctx).await {
        Ok(()) => checks.push(DiagnosticCheck::pass(
            "Memory",
            format!("provider '{}' reachable", prov.name()),
        )),
        Err(e) => checks.push(
            DiagnosticCheck::warn("Memory", "provider", format!("{e}")).with_remediation(
                RemediationKind::Manual,
                "memory.provider_unreachable",
                format!(
                    "verify credentials/endpoint: `borg settings get memory.{selected}.api_key`"
                ),
            ),
        ),
    }
}

pub(in crate::doctor) async fn check_memory(
    config: &Config,
    checks: &mut Vec<DiagnosticCheck>,
    probe_network: bool,
) {
    check_memory_provider(config, checks, probe_network).await;
    match Database::open().await {
        Ok(db) => match db.list_memory_entries("global").await {
            Ok(entries) => {
                let has_index = entries.iter().any(|e| e.name == "INDEX");
                let total = entries.len();
                if has_index {
                    checks.push(DiagnosticCheck::pass(
                        "Memory",
                        format!("{total} memory entries (INDEX present)"),
                    ));
                } else if total == 0 {
                    checks.push(DiagnosticCheck::warn(
                        "Memory",
                        "memory entries",
                        "no entries in DB yet",
                    ));
                } else {
                    checks.push(
                        DiagnosticCheck::warn("Memory", "memory index", "INDEX entry missing")
                            .with_remediation(
                                RemediationKind::AutoSafe,
                                "memory.ensure_global_index",
                                "run /heal to seed global/INDEX from existing entries",
                            ),
                    );
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::fail(
                    "Memory",
                    "list memory entries",
                    format!("{e}"),
                ));
            }
        },
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Memory",
                "open database",
                format!("{e}"),
            ));
        }
    }

    match Config::identity_path() {
        Ok(path) => {
            if path.exists() {
                checks.push(DiagnosticCheck::pass("Memory", "IDENTITY.md exists"));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Memory",
                    "IDENTITY.md exists",
                    "not found",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Memory",
                IDENTITY_FILE,
                format!("{e}"),
            ));
        }
    }
}

fn classify_embedding_lag(global_count: usize, entry_count: usize) -> DiagnosticCheck {
    let detail = format!("{global_count}/{entry_count} memory entries indexed");
    if entry_count == 0 || global_count >= entry_count {
        return DiagnosticCheck::pass("Embeddings", format!("whole-entry coverage: {detail}"));
    }
    let gap = entry_count - global_count;
    if gap <= EMBEDDING_LAG_WARN_THRESHOLD {
        DiagnosticCheck::pass(
            "Embeddings",
            format!("whole-entry coverage: {detail} (pending backfill)"),
        )
    } else {
        DiagnosticCheck::warn(
            "Embeddings",
            "whole-entry coverage",
            format!("{detail} — {gap} entries lag behind"),
        )
        .with_remediation(
            RemediationKind::AutoSafe,
            "embeddings.repair",
            "run /heal to backfill stale or missing embeddings",
        )
    }
}

#[derive(Default)]
struct ChunkCoverage {
    total_entries: usize,
    fully_indexed_entries: usize,
    entries_missing_chunks: usize,
    entries_missing_chunk_embeddings: usize,
}

fn classify_chunk_coverage(coverage: ChunkCoverage) -> DiagnosticCheck {
    let detail = format!(
        "{}/{} entries fully chunk-indexed",
        coverage.fully_indexed_entries, coverage.total_entries
    );
    if coverage.total_entries == 0 || coverage.fully_indexed_entries == coverage.total_entries {
        return DiagnosticCheck::pass("Embeddings", format!("chunk coverage: {detail}"));
    }
    let mut issues = Vec::new();
    if coverage.entries_missing_chunks > 0 {
        issues.push(format!(
            "{} missing chunk rows",
            coverage.entries_missing_chunks
        ));
    }
    if coverage.entries_missing_chunk_embeddings > 0 {
        issues.push(format!(
            "{} missing chunk embeddings",
            coverage.entries_missing_chunk_embeddings
        ));
    }
    DiagnosticCheck::warn(
        "Embeddings",
        "chunk coverage",
        format!("{detail} — {}", issues.join(", ")),
    )
    .with_remediation(
        RemediationKind::AutoSafe,
        "embeddings.repair",
        "run /heal to rebuild stale chunk indexes",
    )
}

async fn collect_chunk_coverage(db: &Database, scopes: &[String]) -> ChunkCoverage {
    let mut coverage = ChunkCoverage::default();
    for scope in scopes {
        let Ok(entries) = db.list_memory_entries(scope).await else {
            continue;
        };
        for entry in entries {
            coverage.total_entries += 1;
            let Ok(chunks) = db.get_chunks_for_file(scope, &entry.name).await else {
                coverage.entries_missing_chunks += 1;
                continue;
            };
            if chunks.is_empty() {
                coverage.entries_missing_chunks += 1;
                continue;
            }
            if chunks.iter().any(|c| c.embedding.is_none()) {
                coverage.entries_missing_chunk_embeddings += 1;
                continue;
            }
            coverage.fully_indexed_entries += 1;
        }
    }
    coverage
}

pub(in crate::doctor) async fn check_embeddings(
    config: &Config,
    checks: &mut Vec<DiagnosticCheck>,
) {
    if !config.memory.embeddings.enabled {
        checks.push(DiagnosticCheck::warn(
            "Embeddings",
            "semantic memory search",
            "disabled in config",
        ));
        return;
    }

    match crate::memory::embeddings::EmbeddingProvider::from_config(&config.memory.embeddings) {
        Some(provider) => {
            checks.push(DiagnosticCheck::pass(
                "Embeddings",
                format!(
                    "provider: {} (model: {})",
                    provider.endpoint, provider.model
                ),
            ));
        }
        None => {
            checks.push(DiagnosticCheck::warn(
                "Embeddings",
                "embedding provider",
                "no embedding-capable API key found, using recency-based loading",
            ));
            return;
        }
    }

    match Database::open().await {
        Ok(db) => {
            let scopes = {
                let mut scopes = crate::memory::resolved_memory_scopes(config);
                if let Ok(mut db_scopes) = db.list_memory_scopes().await {
                    scopes.append(&mut db_scopes);
                }
                scopes.sort();
                scopes.dedup();
                scopes
            };
            let mut global_count = 0usize;
            let mut entry_count = 0usize;
            for scope in &scopes {
                global_count += db.count_embeddings(scope).await.unwrap_or(0);
                entry_count += db
                    .list_memory_entries(scope)
                    .await
                    .map(|v| v.len())
                    .unwrap_or(0);
            }

            checks.push(classify_embedding_lag(global_count, entry_count));
            checks.push(classify_chunk_coverage(
                collect_chunk_coverage(&db, &scopes).await,
            ));
        }
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Embeddings",
                "embedding count",
                format!("database unavailable: {e}"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::CheckStatus;

    #[test]
    fn classify_embedding_lag_warns_with_auto_fix_metadata() {
        let check = classify_embedding_lag(1, 10);
        assert_eq!(check.name, "whole-entry coverage");
        assert_eq!(check.remediation_kind, RemediationKind::AutoSafe);
        assert_eq!(check.remediation_key, Some("embeddings.repair"));
    }

    #[test]
    fn classify_chunk_coverage_warns_when_entries_lack_chunks() {
        let check = classify_chunk_coverage(ChunkCoverage {
            total_entries: 3,
            fully_indexed_entries: 1,
            entries_missing_chunks: 1,
            entries_missing_chunk_embeddings: 1,
        });
        assert_eq!(check.name, "chunk coverage");
        assert!(matches!(check.status, CheckStatus::Warn(_)));
        assert_eq!(check.remediation_kind, RemediationKind::AutoSafe);
    }

    #[test]
    fn classify_embedding_lag_thresholds() {
        // (global, entries, expected_status_label, expect_pending_suffix)
        let cases: &[(usize, usize, &str, bool)] = &[
            (0, 0, "pass", false),   // empty DB
            (5, 5, "pass", false),   // fully indexed
            (10, 5, "pass", false),  // count > entries (cache > db)
            (0, 1, "pass", true),    // single-entry race window
            (3, 8, "pass", true),    // gap = 5, exactly at threshold
            (2, 8, "warn", false),   // gap = 6, just over threshold
            (0, 100, "warn", false), // dead embedder
        ];
        for (global, entries, want_status, want_pending) in cases {
            let got = classify_embedding_lag(*global, *entries);
            let status = match got.status {
                CheckStatus::Pass => "pass",
                CheckStatus::Warn(_) => "warn",
                CheckStatus::Fail(_) => "fail",
            };
            assert_eq!(
                status, *want_status,
                "({global}/{entries}) expected {want_status}, got {status}"
            );
            assert_eq!(
                got.name.contains("(pending backfill)"),
                *want_pending,
                "({global}/{entries}) pending-suffix mismatch"
            );
        }
    }

    mod memory_provider {
        use super::*;
        use crate::config::media::MemoryProviderKind;

        #[tokio::test]
        async fn passes_for_native() {
            let config = Config::default();
            let mut checks = Vec::new();
            check_memory_provider(&config, &mut checks, true).await;
            assert_eq!(checks.len(), 1);
            assert!(matches!(checks[0].status, CheckStatus::Pass));
            assert!(checks[0].name.contains("native"));
        }

        #[tokio::test]
        async fn warns_when_external_unconfigured() {
            // Selecting Letta without credentials makes the registry fall back
            // to native; the doctor must surface that as a Warn with a
            // remediation pointer rather than silently reporting healthy.
            let mut config = Config::default();
            config.memory.provider = MemoryProviderKind::Letta;
            let mut checks = Vec::new();
            check_memory_provider(&config, &mut checks, true).await;
            assert_eq!(checks.len(), 1);
            assert_eq!(checks[0].category, "Memory");
            assert!(matches!(checks[0].status, CheckStatus::Warn(_)));
            assert_eq!(
                checks[0].remediation_key,
                Some("memory.provider_unconfigured")
            );
        }

        #[tokio::test]
        async fn skips_network_probe_when_unattended() {
            // The maintenance sweep runs with probe_network=false. A *configured*
            // external provider must NOT trigger an outbound health() call —
            // base_url points at an unroutable host, so if the probe fired the
            // check would block/error. Instead we expect a clean "configured"
            // pass with no network I/O.
            use crate::security::secrets_resolve::SecretRef;
            const VAR: &str = "BORG_DOCTOR_TEST_ZEP_KEY";
            std::env::set_var(VAR, "k");
            let mut config = Config::default();
            config.memory.provider = MemoryProviderKind::Zep;
            config.memory.zep.api_key = Some(SecretRef::Env {
                var: VAR.to_string(),
            });
            config.memory.zep.base_url = Some("http://127.0.0.1:1".to_string());
            let mut checks = Vec::new();
            check_memory_provider(&config, &mut checks, false).await;
            std::env::remove_var(VAR);
            assert_eq!(checks.len(), 1);
            assert!(
                matches!(checks[0].status, CheckStatus::Pass),
                "unattended probe-skipped path must pass without network I/O; got {:?}",
                checks[0].status
            );
            assert!(
                checks[0].name.contains("configured"),
                "expected 'configured' (not 'reachable'); got {}",
                checks[0].name
            );
        }
    }
}
