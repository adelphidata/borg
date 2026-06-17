//! Tests for the diagnostics runner and check functions, relocated out of
//! `doctor.rs` to keep the production module focused. `use super::*` resolves
//! against the parent `doctor` module.

    use super::*;

    #[test]
    fn report_format_output() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Provider", "API key set"),
                DiagnosticCheck::pass("Provider", "Provider: openrouter"),
                DiagnosticCheck::fail("Provider", "Claude CLI binary", "not found"),
                DiagnosticCheck::warn("Sandbox", "sandbox-exec", "not available"),
            ],
        };
        let output = report.format();
        assert!(output.contains("Borg Doctor"));
        assert!(output.contains("✓ API key set"));
        assert!(output.contains("✗ Claude CLI binary"));
        assert!(output.contains("⚠ sandbox-exec"));
        assert!(output.contains("2 passed, 1 warning(s), 1 failed"));
    }

    #[test]
    fn db_recovery_check_warns_with_backup_path_and_passes_otherwise() {
        // A recorded recovery surfaces as a Warn naming the preserved backup,
        // so the operator knows their DB was rebuilt and where the original is.
        let notice = crate::db::RecoveryNotice {
            backup_path: std::path::PathBuf::from("/home/u/.borg/borg.db.corrupt-42"),
            new_path: std::path::PathBuf::from("/home/u/.borg/borg.db"),
        };
        let warn = db_recovery_check(Some(notice));
        assert_eq!(warn.category, "Database");
        assert!(matches!(warn.status, CheckStatus::Warn(_)));
        match &warn.status {
            CheckStatus::Warn(msg) => assert!(msg.contains("borg.db.corrupt-42"), "got: {msg}"),
            other => panic!("expected warn, got {other:?}"),
        }
        // No recovery → a quiet pass, no spurious warning.
        let ok = db_recovery_check(None);
        assert!(matches!(ok.status, CheckStatus::Pass));
    }

    #[test]
    fn report_counts() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Test", "pass"),
                DiagnosticCheck::warn("Test", "warn", "w"),
                DiagnosticCheck::fail("Test", "fail", "f"),
            ],
        };
        assert_eq!(report.counts(), (1, 1, 1));
    }

    #[test]
    fn empty_report() {
        let report = DiagnosticReport { checks: vec![] };
        let output = report.format();
        assert!(output.contains("0 passed"));
        assert_eq!(report.counts(), (0, 0, 0));
    }

    #[tokio::test]
    async fn run_diagnostics_produces_checks() {
        let config = Config::default();
        let report = run_diagnostics(&config).await;
        // Should always produce at least the provider checks
        assert!(!report.checks.is_empty());
        // Should have at least provider, sandbox, skills, memory, data categories
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Provider"));
        assert!(categories.contains("Sandbox"));
    }

    #[test]
    fn diagnostic_check_pass_fields() {
        let check = DiagnosticCheck::pass("Provider", "API key set");
        assert_eq!(check.category, "Provider");
        assert_eq!(check.name, "API key set");
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn diagnostic_check_fail_includes_detail() {
        let check = DiagnosticCheck::fail("Provider", "API key", "not found in env");
        assert_eq!(check.category, "Provider");
        assert!(matches!(check.status, CheckStatus::Fail(ref msg) if msg == "not found in env"));
    }

    #[test]
    fn diagnostic_check_warn_includes_detail() {
        let check = DiagnosticCheck::warn("Sandbox", "sandbox-exec", "not available on Linux");
        assert_eq!(check.category, "Sandbox");
        assert!(
            matches!(check.status, CheckStatus::Warn(ref msg) if msg == "not available on Linux")
        );
    }

    #[test]
    fn diagnostic_check_with_remediation_formats_fixability() {
        let check = DiagnosticCheck::warn("Embeddings", "chunk coverage", "backlog")
            .with_remediation(RemediationKind::AutoSafe, "embeddings.repair", "run /heal");
        assert_eq!(check.issue_key(), "Embeddings:chunk coverage");
        assert!(check.format_line().contains("[auto-fixable]"));
    }

    #[test]
    fn report_format_groups_consecutive_same_category() {
        let report = DiagnosticReport {
            checks: vec![
                DiagnosticCheck::pass("Sandbox", "check A"),
                DiagnosticCheck::pass("Sandbox", "check C"),
                DiagnosticCheck::pass("Provider", "check B"),
            ],
        };
        let output = report.format();
        // Consecutive Sandbox checks should appear under a single Sandbox heading
        let sandbox_heading_count = output.matches("\nSandbox\n").count();
        assert_eq!(
            sandbox_heading_count, 1,
            "Consecutive same-category checks should share one heading"
        );
        // check A and check C should both appear before Provider
        let check_a_pos = output.find("check A").unwrap();
        let check_c_pos = output.find("check C").unwrap();
        let provider_pos = output.find("Provider").unwrap();
        assert!(check_a_pos < check_c_pos);
        assert!(check_c_pos < provider_pos);
    }

    #[tokio::test]
    async fn run_diagnostics_includes_browser_category() {
        let config = Config::default();
        let report = run_diagnostics(&config).await;
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Browser"));
    }

    #[test]
    fn browser_disabled_produces_warn() {
        let mut checks = Vec::new();
        let mut config = Config::default();
        config.browser.enabled = false;
        check_browser(&config, &mut checks);
        assert_eq!(checks.len(), 1);
        assert!(matches!(checks[0].status, CheckStatus::Warn(_)));
        assert_eq!(checks[0].category, "Browser");
    }

    #[test]
    fn config_security_secure_defaults_all_pass() {
        let config = Config::default();
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        // All checks should be in the Security category
        assert!(checks.iter().all(|c| c.category == "Security"));
        // Default config is secure — no warnings expected
        let warns: Vec<_> = checks
            .iter()
            .filter(|c| matches!(c.status, CheckStatus::Warn(_)))
            .collect();
        assert!(
            warns.is_empty(),
            "Secure defaults should produce no warnings: {warns:?}"
        );
    }

    #[test]
    fn config_security_open_dm_policy_warns() {
        let mut config = Config::default();
        config.gateway.dm_policy = crate::pairing::DmPolicy::Open;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let dm_check = checks.iter().find(|c| c.name == "DM policy").unwrap();
        assert!(matches!(dm_check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_sandbox_disabled_warns() {
        let mut config = Config::default();
        config.sandbox.enabled = false;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let sandbox_check = checks.iter().find(|c| c.name == "sandbox").unwrap();
        assert!(matches!(sandbox_check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_secret_detection_off_warns() {
        let mut config = Config::default();
        config.security.secret_detection = false;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks
            .iter()
            .find(|c| c.name == "secret detection")
            .unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_empty_blocked_paths_warns() {
        let mut config = Config::default();
        config.security.blocked_paths.clear();
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "blocked paths").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_browser_no_sandbox_warns() {
        let mut config = Config::default();
        config.browser.enabled = true;
        config.browser.no_sandbox = true;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "browser sandbox").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_open_channel_policy_warns() {
        let mut config = Config::default();
        config
            .gateway
            .channel_policies
            .insert("telegram".into(), crate::pairing::DmPolicy::Open);
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks
            .iter()
            .find(|c| c.name == "channel policies")
            .unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(ref msg) if msg.contains("telegram")));
    }

    #[test]
    fn config_security_high_rate_limits_warns() {
        let mut config = Config::default();
        config.security.action_limits.tool_calls_block = 1500;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name == "rate limits").unwrap();
        assert!(matches!(check.status, CheckStatus::Warn(_)));
    }

    #[test]
    fn config_security_budget_set_passes() {
        let mut config = Config::default();
        config.budget.monthly_token_limit = 1_000_000;
        let mut checks = Vec::new();
        check_config_security(&config, &mut checks);
        let check = checks.iter().find(|c| c.name.contains("budget")).unwrap();
        assert!(matches!(check.status, CheckStatus::Pass));
    }

    #[tokio::test]
    async fn run_diagnostics_includes_security_category() {
        let config = Config::default();
        let report = run_diagnostics(&config).await;
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Security"));
    }

    #[test]
    fn format_relative_time_seconds() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 30), "30 seconds ago");
        assert_eq!(format_relative_time(now - 1), "1 second ago");
    }

    #[test]
    fn format_relative_time_minutes() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 60), "1 minute ago");
        assert_eq!(format_relative_time(now - 120), "2 minutes ago");
        assert_eq!(format_relative_time(now - 1800), "30 minutes ago");
    }

    #[test]
    fn format_relative_time_hours() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 3600), "1 hour ago");
        assert_eq!(format_relative_time(now - 7200), "2 hours ago");
    }

    #[test]
    fn format_relative_time_days() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 86400), "1 day ago");
        assert_eq!(format_relative_time(now - 172800), "2 days ago");
    }

    #[test]
    fn format_relative_time_future() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now + 100), "just now");
    }

    #[tokio::test]
    async fn run_diagnostics_includes_heartbeat_category() {
        let config = Config::default();
        let report = run_diagnostics(&config).await;
        let categories: std::collections::HashSet<&str> =
            report.checks.iter().map(|c| c.category).collect();
        assert!(categories.contains("Heartbeat"));
    }

    #[tokio::test]
    async fn heartbeat_check_shows_interval() {
        let config = Config::default();
        let mut checks = Vec::new();
        check_heartbeat(&config, &mut checks).await;
        assert!(checks.iter().any(|c| c.name.contains("interval: 60m")));
    }

    // ── Subprocesses check ──

    fn find_subprocess_check(report: &DiagnosticReport) -> Option<&DiagnosticCheck> {
        report
            .checks
            .iter()
            .find(|c| c.category == "Subprocesses" && c.name == "Long-running children")
    }

    #[tokio::test]
    async fn subprocesses_check_absent_without_registry() {
        // The CLI `borg doctor` path (`run_diagnostics`) has no registry
        // handle and MUST NOT produce a misleading "Pass" for a system
        // it couldn't actually inspect.
        let cfg = Config::default();
        let report = run_diagnostics(&cfg).await;
        assert!(
            find_subprocess_check(&report).is_none(),
            "registry-less doctor must omit the Subprocesses check"
        );
    }

    #[tokio::test]
    async fn subprocesses_check_passes_when_registry_empty() {
        let cfg = Config::default();
        let reg = ProcessRegistry::new();
        let report = run_diagnostics_with_registry(&cfg, Some(&reg)).await;
        let check = find_subprocess_check(&report).expect("present");
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn subprocesses_check_passes_when_all_young() {
        let cfg = Config::default();
        let reg = ProcessRegistry::new();
        let _g = reg.register(
            111,
            crate::process_registry::ProcessKind::RunShell,
            "fresh",
            None,
        );
        let report = run_diagnostics_with_registry(&cfg, Some(&reg)).await;
        let check = find_subprocess_check(&report).expect("present");
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[tokio::test]
    async fn subprocesses_check_warns_at_warn_threshold() {
        // Threshold = 0s means every running entry is "older than the
        // threshold" — that's the easiest way to drive Warn from a test
        // without a fake clock.
        let mut cfg = Config::default();
        cfg.process_registry.orphan_warn_secs = 0;
        cfg.process_registry.orphan_fail_secs = 1_000_000;
        let reg = ProcessRegistry::new();
        let _g = reg.register(
            222,
            crate::process_registry::ProcessKind::RunShell,
            "long-build",
            None,
        );
        let report = run_diagnostics_with_registry(&cfg, Some(&reg)).await;
        let check = find_subprocess_check(&report).expect("present");
        assert!(
            matches!(check.status, CheckStatus::Warn(_)),
            "expected Warn, got {:?}",
            check.status
        );
        assert_eq!(check.remediation_kind, RemediationKind::Manual);
        assert_eq!(check.remediation_key, Some("subprocesses.long_running"));
    }

    #[tokio::test]
    async fn subprocesses_check_fails_at_fail_threshold() {
        let mut cfg = Config::default();
        cfg.process_registry.orphan_warn_secs = 0;
        cfg.process_registry.orphan_fail_secs = 0;
        let reg = ProcessRegistry::new();
        let _g = reg.register(
            333,
            crate::process_registry::ProcessKind::RunShell,
            "really-old",
            None,
        );
        let report = run_diagnostics_with_registry(&cfg, Some(&reg)).await;
        let check = find_subprocess_check(&report).expect("present");
        assert!(
            matches!(check.status, CheckStatus::Fail(_)),
            "expected Fail, got {:?}",
            check.status
        );
    }

    #[tokio::test]
    async fn subprocesses_check_omits_pinned_from_warn_count() {
        // A pinned entry past the warn threshold MUST NOT push the
        // check into Warn — otherwise pinning would be useless.
        let mut cfg = Config::default();
        cfg.process_registry.orphan_warn_secs = 0;
        cfg.process_registry.orphan_fail_secs = 1_000_000;
        let reg = ProcessRegistry::new();
        let g = reg.register(
            444,
            crate::process_registry::ProcessKind::RunShell,
            "pinned-build",
            None,
        );
        reg.pin(g.id()).expect("pin");
        let report = run_diagnostics_with_registry(&cfg, Some(&reg)).await;
        let check = find_subprocess_check(&report).expect("present");
        assert_eq!(
            check.status,
            CheckStatus::Pass,
            "pinned entries must not trigger Warn"
        );
    }

    #[tokio::test]
    async fn subprocesses_check_message_caps_offender_list() {
        // 5 offenders → first 3 named, then "… and 2 more". This keeps
        // the persistent-warning surface scannable.
        let mut cfg = Config::default();
        cfg.process_registry.orphan_warn_secs = 0;
        cfg.process_registry.orphan_fail_secs = 1_000_000;
        let reg = ProcessRegistry::new();
        let mut guards = Vec::new();
        for i in 0..5 {
            guards.push(reg.register(
                1000 + i,
                crate::process_registry::ProcessKind::RunShell,
                format!("c{i}"),
                None,
            ));
        }
        let report = run_diagnostics_with_registry(&cfg, Some(&reg)).await;
        let check = find_subprocess_check(&report).expect("present");
        if let CheckStatus::Warn(msg) = &check.status {
            assert!(msg.contains("… and 2 more"), "message: {msg}");
        } else {
            panic!("expected Warn, got {:?}", check.status);
        }
    }
