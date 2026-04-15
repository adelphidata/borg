//! Tool handler for project CRUD operations.

use std::str::FromStr;

use anyhow::Result;
use serde_json::Value;

use super::{optional_str_param, require_str_param, with_db};
use crate::config::Config;
use crate::db::{Database, ProjectStatus};

/// Handle the `projects` tool. Dispatches by action.
pub fn handle_projects(args: &Value, _config: &Config) -> Result<String> {
    with_db(|db| dispatch_project(args, db))
}

fn dispatch_project(args: &Value, db: &Database) -> Result<String> {
    crate::dispatch_action!(args, {
        "create" => project_create(args, db),
        "list" => project_list(args, db),
        "get" => project_get(args, db),
        "update" => project_update(args, db),
        "archive" => project_archive(args, db),
        "delete" => project_delete(args, db),
    })
}

fn project_create(args: &Value, db: &Database) -> Result<String> {
    let raw_name = require_str_param(args, "name")?;
    let name = raw_name.trim();
    if name.is_empty() {
        return Ok("Project name cannot be empty or whitespace-only.".to_string());
    }
    let description = optional_str_param(args, "description").unwrap_or("");
    let id = uuid::Uuid::new_v4().to_string();

    match db.create_project(&id, name, description) {
        Ok(()) => {
            let short = &id[..8.min(id.len())];
            Ok(format!("Project created: \"{name}\" (id: {short})"))
        }
        Err(e) => Ok(format!("Error creating project: {e}")),
    }
}

fn project_list(args: &Value, db: &Database) -> Result<String> {
    let status_filter = optional_str_param(args, "status");
    match db.list_projects(status_filter) {
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
    }
}

fn project_get(args: &Value, db: &Database) -> Result<String> {
    let id = require_str_param(args, "id")?;
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
                        out.push_str(&format!("\n    [{}] {} (id: {short})", wf.status, wf.title,));
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
}

fn project_update(args: &Value, db: &Database) -> Result<String> {
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
        if let Err(msg) = ProjectStatus::from_str(s) {
            return Ok(msg);
        }
    }

    match db.update_project(id, name, description, status) {
        Ok(true) => {
            let short = &id[..8.min(id.len())];
            Ok(format!("Project {short} updated."))
        }
        Ok(false) => Ok(format!("Project not found: {id}")),
        Err(e) => Ok(format!("Error updating project: {e}")),
    }
}

fn project_archive(args: &Value, db: &Database) -> Result<String> {
    let id = require_str_param(args, "id")?;
    // Disambiguate "not found" vs "already archived"
    match db.get_project(id)? {
        None => Ok(format!("Project not found: {id}")),
        Some(p) if p.status == ProjectStatus::Archived.as_str() => {
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
}

fn project_delete(args: &Value, db: &Database) -> Result<String> {
    let id = require_str_param(args, "id")?;
    match db.delete_project(id) {
        Ok(true) => {
            let short = &id[..8.min(id.len())];
            Ok(format!("Project {short} deleted."))
        }
        Ok(false) => Ok(format!("Project not found: {id}")),
        Err(e) => Ok(format!("Error deleting project: {e}")),
    }
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
    use rusqlite::Connection;
    use serde_json::json;

    fn test_db() -> Database {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        Database::from_connection(conn).expect("init test db")
    }

    fn run(args: serde_json::Value, db: &Database) -> String {
        dispatch_project(&args, db).unwrap()
    }

    #[test]
    fn create_project_requires_name() {
        let db = test_db();
        let result = dispatch_project(&json!({"action": "create"}), &db);
        assert!(result.is_err() || result.unwrap().contains("Missing"));
    }

    #[test]
    fn create_project_success() {
        let db = test_db();
        let result = run(
            json!({"action": "create", "name": "Test Project", "description": "A test"}),
            &db,
        );
        assert!(result.contains("Project created"));
        assert!(result.contains("Test Project"));
    }

    #[test]
    fn list_projects_empty() {
        let db = test_db();
        let result = run(json!({"action": "list"}), &db);
        assert!(result.contains("No projects."));
    }

    #[test]
    fn get_project_not_found() {
        let db = test_db();
        let result = run(json!({"action": "get", "id": "nonexistent"}), &db);
        assert!(result.contains("not found"));
    }

    #[test]
    fn update_project_nothing_to_update() {
        let db = test_db();
        let result = run(json!({"action": "update", "id": "some-id"}), &db);
        assert!(result.contains("Nothing to update"));
    }

    #[test]
    fn update_project_not_found() {
        let db = test_db();
        let result = run(
            json!({"action": "update", "id": "nonexistent", "name": "New Name"}),
            &db,
        );
        assert!(result.contains("not found"));
    }

    #[test]
    fn delete_project_not_found() {
        let db = test_db();
        let result = run(json!({"action": "delete", "id": "nonexistent"}), &db);
        assert!(result.contains("not found"));
    }

    #[test]
    fn archive_project_not_found() {
        let db = test_db();
        let result = run(json!({"action": "archive", "id": "nonexistent"}), &db);
        assert!(result.contains("not found") || result.contains("already archived"));
    }

    #[test]
    fn unknown_action_returns_help() {
        let result = handle_projects(&json!({"action": "nope"}), &Config::default()).unwrap();
        assert!(result.contains("Unknown action"));
    }

    #[test]
    fn missing_action_errors() {
        let result = handle_projects(&json!({}), &Config::default());
        assert!(result.is_err());
    }

    #[test]
    fn create_project_empty_name_rejected() {
        let db = test_db();
        let result = run(json!({"action": "create", "name": ""}), &db);
        assert!(result.contains("cannot be empty"));
    }

    #[test]
    fn create_project_whitespace_name_rejected() {
        let db = test_db();
        let result = run(json!({"action": "create", "name": "   "}), &db);
        assert!(result.contains("cannot be empty"));
    }

    #[test]
    fn update_project_empty_name_rejected() {
        let db = test_db();
        let result = run(
            json!({"action": "update", "id": "some-id", "name": "  "}),
            &db,
        );
        assert!(result.contains("cannot be empty"));
    }

    #[test]
    fn update_project_invalid_status_rejected() {
        let db = test_db();
        let result = run(
            json!({"action": "update", "id": "some-id", "status": "bogus"}),
            &db,
        );
        assert!(result.contains("Invalid status"));
    }

    #[test]
    fn archive_already_archived_returns_distinct_message() {
        let db = test_db();

        // Create a project
        let result = run(json!({"action": "create", "name": "ArchiveTest"}), &db);
        assert!(result.contains("Project created"));

        // Find the full ID
        let projects = db.list_projects(None).unwrap();
        let project = projects.iter().find(|p| p.name == "ArchiveTest").unwrap();
        let full_id = project.id.clone();

        // Archive once
        let result = run(json!({"action": "archive", "id": &full_id}), &db);
        assert!(result.contains("archived"));

        // Archive again — should say "already archived", not "not found"
        let result = run(json!({"action": "archive", "id": &full_id}), &db);
        assert!(result.contains("already archived"));
        assert!(!result.contains("not found"));
    }

    #[test]
    fn create_then_get_then_update_then_delete() {
        let db = test_db();

        // Create
        let result = run(
            json!({"action": "create", "name": "CRUD Test", "description": "Integration test"}),
            &db,
        );
        assert!(result.contains("Project created"));

        // Find the full ID via the same in-memory DB
        let projects = db.list_projects(None).unwrap();
        let project = projects.iter().find(|p| p.name == "CRUD Test").unwrap();
        let full_id = project.id.clone();

        // List should include it
        let result = run(json!({"action": "list"}), &db);
        assert!(result.contains("CRUD Test"));

        // Get
        let result = run(json!({"action": "get", "id": &full_id}), &db);
        assert!(result.contains("CRUD Test"));
        assert!(result.contains("Integration test"));

        // Update
        let result = run(
            json!({"action": "update", "id": &full_id, "name": "Updated Name"}),
            &db,
        );
        assert!(result.contains("updated"));

        // Verify update
        let result = run(json!({"action": "get", "id": &full_id}), &db);
        assert!(result.contains("Updated Name"));

        // Archive
        let result = run(json!({"action": "archive", "id": &full_id}), &db);
        assert!(result.contains("archived"));

        // Delete
        let result = run(json!({"action": "delete", "id": &full_id}), &db);
        assert!(result.contains("deleted"));

        // Verify gone
        let result = run(json!({"action": "get", "id": &full_id}), &db);
        assert!(result.contains("not found"));
    }
}
