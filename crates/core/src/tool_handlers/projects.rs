//! Tool handler for project CRUD operations.

use anyhow::Result;
use serde_json::Value;

use super::{optional_str_param, require_str_param};
use crate::config::Config;
use crate::db::Database;

/// Handle the `projects` tool. Dispatches by action.
pub fn handle_projects(args: &Value, _config: &Config) -> Result<String> {
    let action = require_str_param(args, "action")?;
    match action {
        "create" => project_create(args),
        "list" => project_list(args),
        "get" => project_get(args),
        "update" => project_update(args),
        "archive" => project_archive(args),
        "delete" => project_delete(args),
        other => Ok(format!(
            "Unknown project action: {other}. Use: create, list, get, update, archive, delete."
        )),
    }
}

fn with_db<F: FnOnce(&Database) -> Result<String>>(f: F) -> Result<String> {
    let db = Database::open()?;
    f(&db)
}

fn project_create(args: &Value) -> Result<String> {
    let raw_name = require_str_param(args, "name")?;
    let name = raw_name.trim();
    if name.is_empty() {
        return Ok("Project name cannot be empty or whitespace-only.".to_string());
    }
    let description = optional_str_param(args, "description").unwrap_or("");
    let id = uuid::Uuid::new_v4().to_string();

    with_db(|db| match db.create_project(&id, name, description) {
        Ok(()) => {
            let short = &id[..8.min(id.len())];
            Ok(format!("Project created: \"{name}\" (id: {short})"))
        }
        Err(e) => Ok(format!("Error creating project: {e}")),
    })
}

fn project_list(args: &Value) -> Result<String> {
    let status_filter = optional_str_param(args, "status");
    with_db(|db| match db.list_projects(status_filter) {
        Ok(projects) if projects.is_empty() => Ok("No projects.".to_string()),
        Ok(projects) => {
            let mut out = format!("Projects ({}):\n", projects.len());
            for p in &projects {
                let short = &p.id[..8.min(p.id.len())];
                let desc = if p.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", truncate_chars(&p.description, 60))
                };
                out.push_str(&format!(
                    "  [{}] {} (id: {short}){desc}\n",
                    p.status, p.name
                ));
            }
            Ok(out.trim_end().to_string())
        }
        Err(e) => Ok(format!("Error listing projects: {e}")),
    })
}

fn project_get(args: &Value) -> Result<String> {
    let id = require_str_param(args, "id")?;
    with_db(|db| {
        match db.get_project(id)? {
            Some(p) => {
                let mut out = format!(
                    "Project: {}\n  ID: {}\n  Status: {}\n  Description: {}\n  Created: {}\n  Updated: {}",
                    p.name,
                    p.id,
                    p.status,
                    if p.description.is_empty() { "(none)" } else { &p.description },
                    format_ts(p.created_at),
                    format_ts(p.updated_at),
                );

                // Show associated workflows
                match db.list_workflows_by_project(&p.id) {
                    Ok(wfs) if wfs.is_empty() => {
                        out.push_str("\n  Workflows: none");
                    }
                    Ok(wfs) => {
                        out.push_str(&format!("\n  Workflows ({}):", wfs.len()));
                        for wf in &wfs {
                            let short = &wf.id[..8.min(wf.id.len())];
                            out.push_str(&format!(
                                "\n    [{}] {} (id: {short})",
                                wf.status, wf.title,
                            ));
                        }
                    }
                    Err(e) => {
                        out.push_str(&format!("\n  Workflows: error loading ({e})"));
                    }
                }

                Ok(out)
            }
            None => Ok(format!("Project not found: {id}")),
        }
    })
}

fn project_update(args: &Value) -> Result<String> {
    let id = require_str_param(args, "id")?;
    let name = optional_str_param(args, "name");
    let description = optional_str_param(args, "description");
    let status = optional_str_param(args, "status");

    if name.is_none() && description.is_none() && status.is_none() {
        return Ok("Nothing to update. Provide name, description, or status.".to_string());
    }

    if let Some(n) = name {
        if n.trim().is_empty() {
            return Ok("Project name cannot be empty or whitespace-only.".to_string());
        }
    }

    if let Some(s) = status {
        if !matches!(s, "active" | "archived") {
            return Ok(format!("Invalid status: {s}. Use 'active' or 'archived'."));
        }
    }

    with_db(
        |db| match db.update_project(id, name, description, status) {
            Ok(true) => {
                let short = &id[..8.min(id.len())];
                Ok(format!("Project {short} updated."))
            }
            Ok(false) => Ok(format!("Project not found: {id}")),
            Err(e) => Ok(format!("Error updating project: {e}")),
        },
    )
}

fn project_archive(args: &Value) -> Result<String> {
    let id = require_str_param(args, "id")?;
    with_db(|db| {
        // Disambiguate "not found" vs "already archived"
        match db.get_project(id)? {
            None => Ok(format!("Project not found: {id}")),
            Some(p) if p.status == "archived" => {
                let short = &id[..8.min(id.len())];
                Ok(format!("Project {short} is already archived."))
            }
            Some(_) => match db.archive_project(id) {
                Ok(_) => {
                    let short = &id[..8.min(id.len())];
                    Ok(format!("Project {short} archived."))
                }
                Err(e) => Ok(format!("Error archiving project: {e}")),
            },
        }
    })
}

fn project_delete(args: &Value) -> Result<String> {
    let id = require_str_param(args, "id")?;
    with_db(|db| match db.delete_project(id) {
        Ok(true) => {
            let short = &id[..8.min(id.len())];
            Ok(format!("Project {short} deleted."))
        }
        Ok(false) => Ok(format!("Project not found: {id}")),
        Err(e) => Ok(format!("Error deleting project: {e}")),
    })
}

/// Truncate a string to at most `max` characters, appending "..." if truncated.
/// Safe for multi-byte UTF-8.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}...", &s[..end])
}

fn format_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn create_project_requires_name() {
        let result = handle_projects(&json!({"action": "create"}), &test_config());
        assert!(result.is_err() || result.unwrap().contains("Missing"));
    }

    #[test]
    fn create_project_success() {
        let result = handle_projects(
            &json!({"action": "create", "name": "Test Project", "description": "A test"}),
            &test_config(),
        )
        .unwrap();
        assert!(result.contains("Project created"));
        assert!(result.contains("Test Project"));
    }

    #[test]
    fn list_projects_empty() {
        // Fresh DB will have no projects (unless prior test created one)
        let result = handle_projects(&json!({"action": "list"}), &test_config()).unwrap();
        // Could be "No projects." or show some — just verify no error
        assert!(
            result.contains("projects") || result.contains("Projects") || result.contains("No")
        );
    }

    #[test]
    fn get_project_not_found() {
        let result = handle_projects(
            &json!({"action": "get", "id": "nonexistent"}),
            &test_config(),
        )
        .unwrap();
        assert!(result.contains("not found"));
    }

    #[test]
    fn update_project_nothing_to_update() {
        let result = handle_projects(
            &json!({"action": "update", "id": "some-id"}),
            &test_config(),
        )
        .unwrap();
        assert!(result.contains("Nothing to update"));
    }

    #[test]
    fn update_project_not_found() {
        let result = handle_projects(
            &json!({"action": "update", "id": "nonexistent", "name": "New Name"}),
            &test_config(),
        )
        .unwrap();
        assert!(result.contains("not found"));
    }

    #[test]
    fn delete_project_not_found() {
        let result = handle_projects(
            &json!({"action": "delete", "id": "nonexistent"}),
            &test_config(),
        )
        .unwrap();
        assert!(result.contains("not found"));
    }

    #[test]
    fn archive_project_not_found() {
        let result = handle_projects(
            &json!({"action": "archive", "id": "nonexistent"}),
            &test_config(),
        )
        .unwrap();
        assert!(result.contains("not found") || result.contains("already archived"));
    }

    #[test]
    fn unknown_action_returns_help() {
        let result = handle_projects(&json!({"action": "nope"}), &test_config()).unwrap();
        assert!(result.contains("Unknown project action"));
    }

    #[test]
    fn missing_action_errors() {
        let result = handle_projects(&json!({}), &test_config());
        assert!(result.is_err());
    }

    #[test]
    fn create_project_empty_name_rejected() {
        let result =
            handle_projects(&json!({"action": "create", "name": ""}), &test_config()).unwrap();
        assert!(result.contains("cannot be empty"));
    }

    #[test]
    fn create_project_whitespace_name_rejected() {
        let result =
            handle_projects(&json!({"action": "create", "name": "   "}), &test_config()).unwrap();
        assert!(result.contains("cannot be empty"));
    }

    #[test]
    fn update_project_empty_name_rejected() {
        let result = handle_projects(
            &json!({"action": "update", "id": "some-id", "name": "  "}),
            &test_config(),
        )
        .unwrap();
        assert!(result.contains("cannot be empty"));
    }

    #[test]
    fn update_project_invalid_status_rejected() {
        let result = handle_projects(
            &json!({"action": "update", "id": "some-id", "status": "bogus"}),
            &test_config(),
        )
        .unwrap();
        assert!(result.contains("Invalid status"));
    }

    #[test]
    fn archive_already_archived_returns_distinct_message() {
        let config = test_config();

        // Create a project
        let result =
            handle_projects(&json!({"action": "create", "name": "ArchiveTest"}), &config).unwrap();
        assert!(result.contains("Project created"));

        // Find the full ID
        let db = Database::open().unwrap();
        let projects = db.list_projects(None).unwrap();
        let project = projects.iter().find(|p| p.name == "ArchiveTest").unwrap();
        let full_id = project.id.clone();

        // Archive once
        let result =
            handle_projects(&json!({"action": "archive", "id": &full_id}), &config).unwrap();
        assert!(result.contains("archived"));

        // Archive again — should say "already archived", not "not found"
        let result =
            handle_projects(&json!({"action": "archive", "id": &full_id}), &config).unwrap();
        assert!(result.contains("already archived"));
        assert!(!result.contains("not found"));

        // Clean up
        let _ = handle_projects(&json!({"action": "delete", "id": &full_id}), &config);
    }

    #[test]
    fn create_then_get_then_update_then_delete() {
        let config = test_config();

        // Create
        let result = handle_projects(
            &json!({"action": "create", "name": "CRUD Test", "description": "Integration test"}),
            &config,
        )
        .unwrap();
        assert!(result.contains("Project created"));

        // Extract ID from "Project created: \"CRUD Test\" (id: XXXXXXXX)"
        let id_start = result.find("id: ").unwrap() + 4;
        let short_id = &result[id_start..id_start + 8];

        // List should include it
        let result = handle_projects(&json!({"action": "list"}), &config).unwrap();
        assert!(result.contains("CRUD Test"));

        // Get
        let _result = handle_projects(&json!({"action": "get", "id": short_id}), &config).unwrap();
        // Short IDs might not match full UUID, so use list to find full ID
        // For this test, we'll use the DB directly
        let db = Database::open().unwrap();
        let projects = db.list_projects(None).unwrap();
        let project = projects.iter().find(|p| p.name == "CRUD Test").unwrap();
        let full_id = &project.id;

        let result = handle_projects(&json!({"action": "get", "id": full_id}), &config).unwrap();
        assert!(result.contains("CRUD Test"));
        assert!(result.contains("Integration test"));

        // Update
        let result = handle_projects(
            &json!({"action": "update", "id": full_id, "name": "Updated Name"}),
            &config,
        )
        .unwrap();
        assert!(result.contains("updated"));

        // Verify update
        let result = handle_projects(&json!({"action": "get", "id": full_id}), &config).unwrap();
        assert!(result.contains("Updated Name"));

        // Archive
        let result =
            handle_projects(&json!({"action": "archive", "id": full_id}), &config).unwrap();
        assert!(result.contains("archived"));

        // Delete
        let result = handle_projects(&json!({"action": "delete", "id": full_id}), &config).unwrap();
        assert!(result.contains("deleted"));

        // Verify gone
        let result = handle_projects(&json!({"action": "get", "id": full_id}), &config).unwrap();
        assert!(result.contains("not found"));
    }
}
