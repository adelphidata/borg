#[cfg(test)]
macro_rules! integration_handle_tests {
    ($module:ident, $credential:expr) => {
        #[tokio::test]
        async fn handle_missing_credential() {
            let config = crate::config::Config::default();
            let args = serde_json::json!({"action": "test"});
            let result = super::handle(&args, &config).await;
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains(&format!("{} not configured", $credential)),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn handle_unknown_action() {
            let mut config = crate::config::Config::default();
            let env_var = format!("__BORG_TEST_{}__", stringify!($module).to_uppercase());
            config.credentials.insert(
                $credential.to_string(),
                crate::config::CredentialValue::EnvVar(env_var.clone()),
            );
            unsafe {
                std::env::set_var(&env_var, "fake-token");
            }
            let args = serde_json::json!({"action": "nonexistent_action"});
            let result = super::handle(&args, &config).await;
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("Unknown action: nonexistent_action"),
                "unexpected error: {err}"
            );
        }

        #[tokio::test]
        async fn handle_missing_action_param() {
            let mut config = crate::config::Config::default();
            let env_var = format!("__BORG_TEST_{}__", stringify!($module).to_uppercase());
            config.credentials.insert(
                $credential.to_string(),
                crate::config::CredentialValue::EnvVar(env_var.clone()),
            );
            unsafe {
                std::env::set_var(&env_var, "fake-token");
            }
            let args = serde_json::json!({});
            let result = super::handle(&args, &config).await;
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("Missing 'action' parameter"),
                "unexpected error: {err}"
            );
        }
    };
}

pub mod gmail;
pub mod google_calendar;
pub(crate) mod http;
pub mod linear;
pub mod notion;

use anyhow::{bail, Result};
use serde_json::Value;

/// Extract a required string argument, returning a consistent error message.
pub fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str> {
    args[field]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing '{field}'"))
}

/// Format an array of JSON values into a list string with a header.
/// `header_fmt` receives the item count and returns the header line.
pub fn format_list<'a, I, H, F>(items: I, header_fmt: H, no_results: &str, format_item: F) -> String
where
    I: IntoIterator<Item = &'a Value>,
    H: FnOnce(usize) -> String,
    F: Fn(&Value) -> String,
{
    let summaries: Vec<String> = items.into_iter().map(format_item).collect();
    if summaries.is_empty() {
        no_results.to_string()
    } else {
        format!("{}\n{}", header_fmt(summaries.len()), summaries.join("\n"))
    }
}

pub fn resolve_credential_and_action<'a>(
    arguments: &'a Value,
    config: &crate::config::Config,
    credential_name: &str,
) -> Result<(reqwest::Client, String, &'a str)> {
    let token = config
        .resolve_credential_or_env(credential_name)
        .ok_or_else(|| anyhow::anyhow!("{credential_name} not configured"))?;
    let action = arguments["action"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'action' parameter"))?;
    Ok((reqwest::Client::new(), token, action))
}

/// Validate that an API resource ID contains only safe characters
/// (alphanumeric, hyphens, underscores, dots). Prevents path traversal
/// when IDs are interpolated into API URLs.
pub fn validate_resource_id<'a>(id: &'a str, field_name: &str) -> Result<&'a str> {
    if id.is_empty() {
        bail!("Empty {field_name}");
    }
    if id
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        Ok(id)
    } else {
        bail!("Invalid {field_name}: contains illegal characters");
    }
}

/// Check whether an integration's credential is available.
pub fn is_credential_available(config: &crate::config::Config, credential_name: &str) -> bool {
    config.resolve_credential_or_env(credential_name).is_some()
}

macro_rules! integration_dispatch {
    ($($name:literal => $module:ident => $cred:literal),+ $(,)?) => {
        /// Collect integration tool definitions, only including those whose credentials resolve.
        pub fn enabled_tool_definitions(config: &crate::config::Config) -> Vec<crate::types::ToolDefinition> {
            let mut defs = Vec::new();
            $(
                if is_credential_available(config, $cred) {
                    defs.push($module::tool_definition());
                }
            )+
            defs
        }

        /// Collect all integration tool definitions regardless of credentials.
        pub fn all_tool_definitions() -> Vec<crate::types::ToolDefinition> {
            vec![$($module::tool_definition()),+]
        }

        /// Dispatch a tool call to the appropriate integration handler.
        /// Returns `None` if the tool name doesn't match any integration.
        pub async fn dispatch_tool_call(
            tool_name: &str,
            arguments: &Value,
            config: &crate::config::Config,
        ) -> Option<Result<String>> {
            match tool_name {
                $($name => Some($module::handle(arguments, config).await),)+
                _ => None,
            }
        }
    };
}

integration_dispatch! {
    "gmail" => gmail => "GMAIL_API_KEY",
    "google_calendar" => google_calendar => "GOOGLE_CALENDAR_TOKEN",
    "notion" => notion => "NOTION_API_KEY",
    "linear" => linear => "LINEAR_API_KEY",
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_resource_id_accepts_valid() {
        assert!(validate_resource_id("abc-123_def.456", "test").is_ok());
        assert!(validate_resource_id("SM1234567890ABCDE", "sid").is_ok());
    }

    #[test]
    fn validate_resource_id_rejects_traversal() {
        assert!(validate_resource_id("../../admin", "id").is_err());
        assert!(validate_resource_id("foo?bar=1", "id").is_err());
        assert!(validate_resource_id("id/nested", "id").is_err());
        assert!(validate_resource_id("", "id").is_err());
    }

    #[test]
    fn validate_resource_id_allows_dots_hyphens_underscores() {
        assert_eq!(validate_resource_id("a", "id").unwrap(), "a");
        assert_eq!(validate_resource_id("x-y_z.0", "id").unwrap(), "x-y_z.0");
        assert_eq!(
            validate_resource_id("a".repeat(200).as_str(), "id").unwrap(),
            "a".repeat(200)
        );
        assert_eq!(validate_resource_id(".", "id").unwrap(), ".");
        assert_eq!(validate_resource_id("_", "id").unwrap(), "_");
        assert_eq!(validate_resource_id("-", "id").unwrap(), "-");
    }

    #[test]
    fn all_tool_definitions_are_valid() {
        let defs = all_tool_definitions();
        let mut names = std::collections::HashSet::new();
        for def in &defs {
            assert!(!def.function.name.is_empty(), "tool name must not be empty");
            assert!(
                !def.function.description.is_empty(),
                "tool description must not be empty for {}",
                def.function.name
            );
            assert!(
                names.insert(&def.function.name),
                "duplicate tool name: {}",
                def.function.name
            );
        }
    }

    #[test]
    fn enabled_tool_definitions_empty_without_credentials() {
        let config = crate::config::Config::default();
        let defs = enabled_tool_definitions(&config);
        // Without any credentials configured, no integrations should appear
        assert!(defs.is_empty());
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_none() {
        let config = crate::config::Config::default();
        let args = serde_json::json!({});
        assert!(dispatch_tool_call("nonexistent", &args, &config)
            .await
            .is_none());
        assert!(dispatch_tool_call("", &args, &config).await.is_none());
        assert!(dispatch_tool_call("gmail_extended", &args, &config)
            .await
            .is_none());
    }
}
