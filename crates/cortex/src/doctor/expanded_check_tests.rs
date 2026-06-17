//! Tests for the diagnostics runner and check functions, relocated out of
//! `doctor.rs` to keep the production module focused. `use super::*` resolves
//! against the parent `doctor` module.

    use super::*;
    use crate::db::NewTask;

    fn fails(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Fail(_))
    }
    fn warns(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Warn(_))
    }
    fn passes(c: &DiagnosticCheck) -> bool {
        matches!(c.status, CheckStatus::Pass)
    }

    #[test]
    fn settings_check_passes_default_and_fails_out_of_range() {
        let mut checks = Vec::new();
        check_settings(&Config::default(), &mut checks);
        assert_eq!(checks.len(), 1);
        assert!(passes(&checks[0]), "valid defaults must pass");

        let mut bad = Config::default();
        bad.llm.temperature = 5.0; // validate() bails outside 0.0..=2.0
        let mut checks = Vec::new();
        check_settings(&bad, &mut checks);
        assert!(fails(&checks[0]), "out-of-range temperature must fail");
        assert_eq!(checks[0].remediation_kind, RemediationKind::Manual);
    }

    #[test]
    fn keychain_classification_reflects_backend_and_file_presence() {
        assert!(passes(&classify_keychain(KekBackend::OsKeychain, false)));
        assert!(passes(&classify_keychain(KekBackend::Auto, false)));
        assert!(
            warns(&classify_keychain(KekBackend::File, true)),
            "file backend with blob present → reduced-posture warning",
        );
        assert!(
            fails(&classify_keychain(KekBackend::File, false)),
            "file backend with missing blob → fail (cannot unseal)",
        );
    }

    #[test]
    fn orphan_lock_check_is_autosafe_with_repair_key() {
        let c = orphan_lock_check(420);
        assert!(warns(&c));
        assert_eq!(c.remediation_kind, RemediationKind::AutoSafe);
        assert_eq!(c.remediation_key, Some("locks.clear_orphan"));
        let CheckStatus::Warn(msg) = &c.status else {
            unreachable!()
        };
        assert!(msg.contains("420"), "message names the idle age: {msg}");
    }

    #[test]
    fn service_check_reports_only_when_installed() {
        assert!(
            classify_service(None).is_none(),
            "no unit file → no finding (foreground is normal)",
        );
        let p = std::path::Path::new("/etc/systemd/system/borg.service");
        let c = classify_service(Some(p)).expect("installed → some");
        assert!(
            passes(&c),
            "installed service is informational, never a failure"
        );
    }

    #[test]
    fn tunnel_classification_gates_on_provider_and_binary() {
        // Applicability lives in tunnel_binary: no tunnel / custom command have
        // no fixed binary, so check_tunnel emits nothing for them.
        assert!(tunnel_binary(TunnelProvider::None).is_none());
        assert!(tunnel_binary(TunnelProvider::Custom).is_none());
        assert_eq!(
            tunnel_binary(TunnelProvider::Cloudflare),
            Some("cloudflared")
        );
        // Provider binary missing → fail; present → pass.
        let missing = classify_tunnel(TunnelProvider::Ngrok, "ngrok", false);
        assert!(fails(&missing));
        assert_eq!(missing.remediation_kind, RemediationKind::Manual);
        assert!(passes(&classify_tunnel(
            TunnelProvider::Ngrok,
            "ngrok",
            true
        )));
    }

    #[test]
    fn mcp_label_maps_states_without_false_positives() {
        assert!(fails(&mcp_check_for_label("srv", "auth-failed")));
        assert!(warns(&mcp_check_for_label("srv", "reconnecting")));
        assert!(warns(&mcp_check_for_label("srv", "degraded")));
        assert!(passes(&mcp_check_for_label("srv", "connected")));
        // The CLI has no live registry — "not-started" must NOT be a problem.
        assert!(passes(&mcp_check_for_label("srv", "not-started")));
    }

    #[test]
    fn mcp_check_emits_nothing_without_configured_servers() {
        let mut checks = Vec::new();
        check_mcp(&Config::default(), &mut checks);
        assert!(
            checks.is_empty(),
            "no configured MCP servers → no rows (suppressed section)",
        );
    }

    #[test]
    fn docker_classification_gates_on_use_and_health() {
        // Applicability: the docker skill ships disabled, so a vanilla install
        // is not "in use" and check_docker emits nothing.
        assert!(
            !docker_in_use(&Config::default()),
            "docker skill ships disabled"
        );
        // Severity mapping (only reached once in-use):
        assert!(warns(&classify_docker(false, false)), "no binary → warn");
        assert!(warns(&classify_docker(true, false)), "daemon down → warn");
        assert!(passes(&classify_docker(true, true)), "healthy → pass");
    }

    #[tokio::test]
    async fn scheduled_tasks_warns_only_on_errored_tasks() {
        let db = Database::test_db().await;

        // Clean store: summary pass, no warning.
        let clean = scheduled_task_checks(&db).await;
        assert!(clean.iter().all(passes), "clean store → all pass");
        assert!(
            clean.iter().any(|c| c.name.contains("scheduled task(s)")),
            "must report a task count summary",
        );

        // Seed a task whose last run errored.
        db.create_task(&NewTask {
            id: "boom",
            name: "flaky job",
            prompt: "do stuff",
            schedule_type: "interval",
            schedule_expr: "1h",
            timezone: "local",
            next_run: Some(100),
            max_retries: Some(3),
            timeout_ms: None,
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
        })
        .await
        .expect("create task");
        db.set_task_retry("boom", 1, "connection refused", 0)
            .await
            .expect("set last_error");

        let after = scheduled_task_checks(&db).await;
        let err_warn = after
            .iter()
            .find(|c| c.name == "tasks with errors")
            .expect("errored-task warning present");
        assert!(warns(err_warn), "a task with last_error must warn");
        let CheckStatus::Warn(msg) = &err_warn.status else {
            unreachable!()
        };
        assert!(
            msg.contains("flaky job"),
            "warning names the offending task: {msg}"
        );
    }
