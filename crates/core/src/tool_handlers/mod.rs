mod filesystem;
mod list;
mod media;
mod memory;
mod projects;
mod schedule;
mod scripts;
mod shell;
pub mod user_input;
mod web;

use anyhow::Result;

use crate::db::Database;

// ── Shared helpers ──

/// Return an early-exit string if the feature is disabled.
pub(crate) fn check_enabled(enabled: bool, feature: &str) -> Option<String> {
    if enabled {
        None
    } else {
        Some(format!(
            "{feature} is disabled. Enable: {feature}.enabled = true"
        ))
    }
}

/// Open the database and run a callback, formatting the open error if it fails.
pub(crate) fn with_db<F>(f: F) -> Result<String>
where
    F: FnOnce(&Database) -> Result<String>,
{
    match Database::open() {
        Ok(db) => f(&db),
        Err(e) => {
            tracing::warn!("Failed to open database: {e}");
            Ok(format!("Error opening database: {e}"))
        }
    }
}

// ── Tool-call argument extraction ──
//
// The canonical API for reading tool-call arguments. All handlers use these
// free functions; a prior `ToolArgs` wrapper struct had no callers and was
// removed.

/// Return the value of a required string parameter, or an error naming it.
pub fn require_str_param<'a>(args: &'a serde_json::Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required parameter '{name}'."))
}

/// Return the value of an optional string parameter, if present.
pub fn optional_str_param<'a>(args: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    args[name].as_str()
}

/// Return the value of an optional u64 parameter, falling back to `default`.
pub fn optional_u64_param(args: &serde_json::Value, name: &str, default: u64) -> u64 {
    args[name].as_u64().unwrap_or(default)
}

/// Return the value of an optional bool parameter, falling back to `default`.
pub fn optional_bool_param(args: &serde_json::Value, name: &str, default: bool) -> bool {
    args[name].as_bool().unwrap_or(default)
}

/// Return the value of an optional i64 parameter, if present and integral.
pub fn optional_i64_param(args: &serde_json::Value, name: &str) -> Option<i64> {
    args[name].as_i64()
}

/// Return the value of an optional f64 parameter, falling back to `default`.
pub fn optional_f64_param(args: &serde_json::Value, name: &str, default: f64) -> f64 {
    args[name].as_f64().unwrap_or(default)
}

// ── Re-exports ──

// Memory
pub use memory::{
    format_search_results, handle_memory_search, handle_read_memory, handle_write_memory,
};

// Filesystem
pub use filesystem::{
    handle_apply_patch_unified, handle_apply_skill_patch, handle_create_channel, handle_list_dir,
    handle_read_file, handle_read_pdf, is_blocked_path,
};

// Shell
pub use shell::handle_run_shell;

// Web
pub use web::{handle_web_fetch, handle_web_search};

// Projects
pub use projects::handle_projects;

// Schedule
pub use schedule::{handle_manage_cron, handle_manage_tasks, handle_schedule, update_task_status};

// Media
pub use media::{handle_browser, handle_generate_image, handle_text_to_speech};

// List
pub use list::{handle_list, handle_list_channels, handle_list_skills};

// Scripts
pub use scripts::{handle_manage_scripts, handle_run_script, handle_security_audit};

// User input
pub use user_input::{handle_request_user_input, handle_update_plan};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tool_definitions::core_tool_definitions;
    use serde_json::json;

    // -- require_str_param --

    #[test]
    fn require_str_param_extracts_string() {
        let args = json!({"name": "hello"});
        assert_eq!(require_str_param(&args, "name").unwrap(), "hello");
    }

    #[test]
    fn require_str_param_missing_key_errors() {
        let args = json!({"other": "value"});
        let result = require_str_param(&args, "name");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing required"));
    }

    #[test]
    fn require_str_param_wrong_type_errors() {
        let args = json!({"name": 42});
        let result = require_str_param(&args, "name");
        assert!(result.is_err());
    }

    #[test]
    fn require_str_param_null_value_errors() {
        let args = json!({"name": null});
        let result = require_str_param(&args, "name");
        assert!(result.is_err());
    }

    #[test]
    fn require_str_param_empty_string_ok() {
        let args = json!({"name": ""});
        assert_eq!(require_str_param(&args, "name").unwrap(), "");
    }

    // -- core_tool_definitions --

    #[test]
    fn core_tool_definitions_includes_base_tools() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"read_memory"));
        assert!(names.contains(&"list"));
        assert!(names.contains(&"apply_patch"));
        assert!(names.contains(&"run_shell"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"list_dir"));
        assert!(names.contains(&"schedule"));
        assert!(!names.contains(&"list_skills"));
        assert!(!names.contains(&"apply_skill_patch"));
    }

    #[test]
    fn core_tool_definitions_excludes_browser_when_disabled() {
        let mut config = Config::default();
        config.browser.enabled = false;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"browser"));
    }

    #[test]
    fn core_tool_definitions_includes_browser_when_enabled() {
        let mut config = Config::default();
        config.browser.enabled = true;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"browser"));
    }

    #[test]
    fn core_tool_definitions_excludes_tts_when_disabled() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"text_to_speech"));
    }

    #[test]
    fn core_tool_definitions_includes_tts_when_enabled() {
        let mut config = Config::default();
        config.tts.enabled = true;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"text_to_speech"));
    }

    #[test]
    fn core_tool_definitions_excludes_web_when_disabled() {
        let mut config = Config::default();
        config.web.enabled = false;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(!names.contains(&"web_fetch"));
        assert!(!names.contains(&"web_search"));
    }

    #[test]
    fn core_tool_definitions_includes_web_when_enabled() {
        let mut config = Config::default();
        config.web.enabled = true;
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"web_fetch"));
        assert!(names.contains(&"web_search"));
    }

    #[test]
    fn core_tool_definitions_all_have_parameters() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        for def in &defs {
            assert!(
                def.function.parameters.is_object(),
                "Tool '{}' should have object parameters",
                def.function.name
            );
        }
    }

    #[test]
    fn core_tool_definitions_all_have_descriptions() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        for def in &defs {
            assert!(
                !def.function.description.is_empty(),
                "Tool '{}' should have a description",
                def.function.name
            );
        }
    }

    #[test]
    fn core_tool_definitions_has_apply_patch_with_target() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        let ap = defs
            .iter()
            .find(|d| d.function.name == "apply_patch")
            .expect("should have apply_patch");
        let params = &ap.function.parameters;
        assert!(
            params["properties"]["target"].is_object(),
            "apply_patch should have 'target' parameter"
        );
    }

    #[test]
    fn core_tool_definitions_has_list_with_what() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        let list = defs
            .iter()
            .find(|d| d.function.name == "list")
            .expect("should have list");
        let params = &list.function.parameters;
        assert!(
            params["properties"]["what"].is_object(),
            "list should have 'what' parameter"
        );
    }

    #[test]
    fn core_tool_definitions_count_reduced() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert_eq!(
            names.len(),
            14,
            "expected 14 core tools (all enabled), got: {names:?}"
        );

        let mut minimal_config = Config::default();
        minimal_config.web.enabled = false;
        minimal_config.browser.enabled = false;
        let defs = core_tool_definitions(&minimal_config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert_eq!(names.len(), 11, "expected 11 base tools, got: {names:?}");
    }

    #[test]
    fn core_tool_definitions_includes_request_user_input() {
        let config = Config::default();
        let defs = core_tool_definitions(&config);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"request_user_input"));
    }
}
