pub mod filesystem;
pub(crate) mod hints;
mod list;
mod media;
mod memory;
mod schedule;
mod scripts;
pub(crate) mod shell;
pub(crate) mod subscribe;
pub mod user_input;
mod web;

use anyhow::Result;

use crate::db::Database;

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

/// Open the database and run an async callback, formatting the open error if it fails.
pub(crate) async fn with_db<F, Fut>(f: F) -> Result<String>
where
    F: FnOnce(Database) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    match Database::open().await {
        Ok(db) => f(db).await,
        Err(e) => {
            tracing::warn!("Failed to open database: {e}");
            Ok(format!("Error opening database: {e}"))
        }
    }
}

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

pub use filesystem::{
    complete_paths, handle_apply_patch_unified, handle_apply_skill_patch, handle_create_channel,
    handle_list_dir, handle_read_file, handle_read_pdf, is_blocked_path, PathSuggestion,
    MAX_PATH_SUGGESTIONS,
};
pub use list::{handle_list, handle_list_channels, handle_list_skills};
pub use media::{handle_browser, handle_generate_image, handle_text_to_speech};
pub(crate) use memory::{effective_batch_scopes, handle_write_memory_batch, native_memory_search};
pub use memory::{
    format_search_results, handle_memory_search, handle_read_memory, handle_write_memory,
};
pub use schedule::{handle_manage_cron, handle_manage_tasks, handle_schedule, update_task_status};
pub use scripts::{handle_manage_scripts, handle_run_script, handle_security_audit};
pub use shell::{handle_run_shell, handle_run_shell_with_context, SHELL_DENIED_PREFIX};
pub use user_input::{handle_request_user_input, handle_update_plan};
pub use web::{handle_web_fetch, handle_web_search};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tool::definitions::core_tool_definitions;
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

    /// Every tool schema Borg sends must also be listed in
    /// `BUILTIN_TOOL_NAMES`, which is what stops an MCP server or WASM plugin
    /// from registering the same name.
    ///
    /// Regression: six real dispatchable tools (`vision_analyze`,
    /// `video_analyze`, `computer_use`, `get_goal`, `create_goal`,
    /// `update_goal`) were missing. A plugin advertising one of those names
    /// passed the dedup filter, so the request carried two tool definitions
    /// with the same name — which Anthropic and OpenAI both reject with a 400,
    /// breaking every turn while the plugin is installed.
    ///
    /// Enumerated from the definitions rather than a hand-written list so a
    /// newly added tool cannot silently reintroduce the gap.
    #[tokio::test]
    async fn every_emitted_tool_name_is_registered_as_a_builtin() {
        // Enable the config-gated tools so their schemas are emitted here.
        let mut config = Config::default();
        config.browser.enabled = true;
        config.web.enabled = true;
        config.tts.enabled = true;

        let defs = core_tool_definitions(&config).await;
        let missing: Vec<String> = defs
            .iter()
            .map(|d| d.function.name.clone())
            .filter(|name| !crate::tool::names::BUILTIN_TOOL_NAMES.contains(&name.as_str()))
            .collect();

        assert!(
            missing.is_empty(),
            "these emitted tools are absent from BUILTIN_TOOL_NAMES, so a plugin \
             or MCP server could register the same name: {missing:?}"
        );
    }

    // -- core_tool_definitions --

    #[tokio::test]
    async fn core_tool_definitions_includes_base_tools() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
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

    async fn assert_tool_gated(
        enabled: bool,
        set_enabled: fn(&mut Config, bool),
        tool_names: &[&str],
    ) {
        let mut config = Config::default();
        set_enabled(&mut config, enabled);
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        for tool in tool_names {
            assert_eq!(
                names.contains(tool),
                enabled,
                "tool `{tool}` presence should match enabled={enabled}"
            );
        }
    }

    #[tokio::test]
    async fn core_tool_definitions_gates_browser_on_config() {
        let setter: fn(&mut Config, bool) = |c, v| c.browser.enabled = v;
        assert_tool_gated(false, setter, &["browser"]).await;
        assert_tool_gated(true, setter, &["browser"]).await;
    }

    #[tokio::test]
    async fn core_tool_definitions_gates_tts_on_config() {
        let setter: fn(&mut Config, bool) = |c, v| c.tts.enabled = v;
        assert_tool_gated(false, setter, &["text_to_speech"]).await;
        assert_tool_gated(true, setter, &["text_to_speech"]).await;
    }

    #[tokio::test]
    async fn core_tool_definitions_gates_web_on_config() {
        let setter: fn(&mut Config, bool) = |c, v| c.web.enabled = v;
        assert_tool_gated(false, setter, &["web_fetch", "web_search"]).await;
        assert_tool_gated(true, setter, &["web_fetch", "web_search"]).await;
    }

    #[tokio::test]
    async fn core_tool_definitions_all_have_parameters() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
        for def in &defs {
            assert!(
                def.function.parameters.is_object(),
                "Tool '{}' should have object parameters",
                def.function.name
            );
        }
    }

    #[tokio::test]
    async fn core_tool_definitions_all_have_descriptions() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
        for def in &defs {
            assert!(
                !def.function.description.is_empty(),
                "Tool '{}' should have a description",
                def.function.name
            );
        }
    }

    #[tokio::test]
    async fn core_tool_definitions_has_apply_patch_with_target() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
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

    #[tokio::test]
    async fn core_tool_definitions_has_list_with_what() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
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

    #[tokio::test]
    async fn core_tool_definitions_count_reduced() {
        // computer_use defaults on only on macOS; pin it so the count is the
        // same on every CI platform.
        let mut config = Config::default();
        config.computer_use.enabled = true;
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert_eq!(
            names.len(),
            19,
            "expected 19 core tools (all enabled: 3 goal tools, vision_analyze, \
             video_analyze, computer_use), got: {names:?}"
        );

        let mut minimal_config = config.clone();
        minimal_config.web.enabled = false;
        minimal_config.browser.enabled = false;
        let defs = core_tool_definitions(&minimal_config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert_eq!(
            names.len(),
            14,
            "expected 14 base tools: full minus web_fetch/web_search/vision_analyze/\
             video_analyze (web-gated) and browser, got: {names:?}"
        );
    }

    #[tokio::test]
    async fn core_tool_definitions_includes_request_user_input() {
        let config = Config::default();
        let defs = core_tool_definitions(&config).await;
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"request_user_input"));
    }
}
