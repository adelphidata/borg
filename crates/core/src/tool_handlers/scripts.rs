use anyhow::Result;
use tracing::instrument;

use crate::config::Config;
use crate::db::Database;

use super::{check_enabled, optional_bool_param, optional_str_param, require_str_param, with_db};

pub fn handle_manage_scripts(args: &serde_json::Value, config: &Config) -> Result<String> {
    if let Some(msg) = check_enabled(config.scripts.enabled, "scripts") {
        return Ok(msg);
    }
    with_db(|db| {
        crate::dispatch_action!(args, {
            "create" => scripts_create(args, config, db),
            "update" => scripts_update(args, db),
            "delete" => scripts_delete(args, db),
            "get" => scripts_get(args, db),
            "list" => crate::scripts::list_scripts(db),
        })
    })
}

fn scripts_create(args: &serde_json::Value, config: &Config, db: &Database) -> Result<String> {
    let name = require_str_param(args, "name")?;
    let patch = require_str_param(args, "patch")?;
    let description = optional_str_param(args, "description").unwrap_or("");
    let runtime = optional_str_param(args, "runtime").unwrap_or("python");
    let entrypoint = optional_str_param(args, "entrypoint").unwrap_or("main.py");
    let sandbox_profile = optional_str_param(args, "sandbox_profile")
        .unwrap_or(&config.scripts.default_sandbox_profile);
    let network_access = optional_bool_param(args, "network_access", false);
    let fs_read: Vec<String> = args["fs_read"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let fs_write: Vec<String> = args["fs_write"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let ephemeral = optional_bool_param(args, "ephemeral", false);

    crate::scripts::create_script(
        db,
        &crate::scripts::CreateScriptParams {
            name,
            description,
            patch,
            runtime,
            entrypoint,
            sandbox_profile,
            network_access,
            fs_read: &fs_read,
            fs_write: &fs_write,
            ephemeral,
            max_scripts: config.scripts.max_scripts,
        },
    )
}

fn scripts_update(args: &serde_json::Value, db: &Database) -> Result<String> {
    let name = require_str_param(args, "name")?;
    let patch = require_str_param(args, "patch")?;
    crate::scripts::update_script(db, name, patch)
}

fn scripts_delete(args: &serde_json::Value, db: &Database) -> Result<String> {
    let name = require_str_param(args, "name")?;
    crate::scripts::delete_script(db, name)
}

fn scripts_get(args: &serde_json::Value, db: &Database) -> Result<String> {
    let name = require_str_param(args, "name")?;
    crate::scripts::get_script(db, name)
}

#[instrument(skip_all, fields(tool.name = "run_script"))]
pub async fn handle_run_script(args: &serde_json::Value, config: &Config) -> Result<String> {
    if let Some(msg) = check_enabled(config.scripts.enabled, "scripts") {
        return Ok(msg);
    }
    let name = require_str_param(args, "name")?;
    let script_args = args.get("args").cloned().unwrap_or(serde_json::json!({}));
    crate::scripts::execute_script(config, name, &script_args.to_string()).await
}

pub fn handle_security_audit(args: &serde_json::Value, config: &Config) -> Result<String> {
    if !config.security.host_audit {
        return Ok(
            "Host audit is disabled. Enable it in config: security.host_audit = true".to_string(),
        );
    }
    use crate::doctor::DiagnosticReport;
    use crate::host_audit;

    let mut audit_checks = Vec::new();
    match args.get("category").and_then(|v| v.as_str()) {
        Some("firewall") => host_audit::check_firewall(&mut audit_checks),
        Some("ports") => host_audit::check_listening_ports(&mut audit_checks),
        Some("ssh") => host_audit::check_ssh_config(&mut audit_checks),
        Some("permissions") => host_audit::check_sensitive_permissions(&mut audit_checks),
        Some("encryption") => host_audit::check_disk_encryption(&mut audit_checks),
        Some("updates") => host_audit::check_os_updates(&mut audit_checks),
        Some("services") => host_audit::check_running_services(&mut audit_checks),
        Some(other) => {
            return Ok(format!(
                "Unknown audit category: {other}. Valid: firewall, ports, ssh, permissions, encryption, updates, services"
            ))
        }
        None => host_audit::run_host_security_checks(&mut audit_checks),
    }
    let report = DiagnosticReport {
        checks: audit_checks,
    };
    Ok(report.format())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn handle_security_audit_disabled() {
        let mut config = Config::default();
        config.security.host_audit = false;
        let result = handle_security_audit(&json!({}), &config).unwrap();
        assert!(result.contains("disabled"));
    }

    #[test]
    fn handle_security_audit_unknown_category() {
        let mut config = Config::default();
        config.security.host_audit = true;
        let result = handle_security_audit(&json!({"category": "invalid"}), &config).unwrap();
        assert!(result.contains("Unknown audit category"));
    }
}
