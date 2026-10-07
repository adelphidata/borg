use super::AgentRole;
use crate::db::Database;
use crate::tool::names;

/// Built-in role definitions.
pub const BUILTIN_ROLES: &[(&str, &str, f32, &[&str])] = &[
    (
        "researcher",
        "Information gathering and analysis. Use this role for tasks that require searching, reading, and synthesizing information.",
        0.3,
        &[
            names::RUN_SHELL,
            names::WEB_FETCH,
            names::WEB_SEARCH,
            names::READ_MEMORY,
            names::WRITE_MEMORY,
        ],
    ),
    (
        "coder",
        "Code writing and modification. Use this role for tasks that require creating or modifying code files.",
        0.2,
        &[names::RUN_SHELL, names::APPLY_PATCH, names::READ_MEMORY],
    ),
    (
        "writer",
        "Documentation and content writing. Use this role for tasks that require writing documentation, notes, or creative content.",
        0.7,
        &[
            names::RUN_SHELL,
            names::APPLY_PATCH,
            names::READ_MEMORY,
            names::WRITE_MEMORY,
            names::WEB_SEARCH,
        ],
    ),
];

pub fn builtin_role(name: &str) -> Option<AgentRole> {
    BUILTIN_ROLES
        .iter()
        .find(|(n, ..)| *n == name)
        .map(|(name, desc, temp, tools)| AgentRole {
            name: name.to_string(),
            description: desc.to_string(),
            model: None,
            provider: None,
            temperature: Some(*temp),
            system_instructions: None,
            tools_allowed: Some(tools.iter().map(ToString::to_string).collect()),
            max_iterations: None,
        })
}

/// Decode a role's stored `tools_allowed` column.
///
/// `None` (nothing stored) stays `None`: no restriction. A stored list is
/// returned as-is, `[]` included. A value that does not decode as a string
/// array fails closed to "no tools" with a warning — treating corrupt JSON as
/// "unrestricted" would hand a locked-down role every tool without a trace.
fn decode_stored_tools_allowed(role: &str, json: Option<String>) -> Option<Vec<String>> {
    let json = json?;
    match serde_json::from_str::<Vec<String>>(&json) {
        Ok(list) => Some(list),
        Err(e) => {
            tracing::warn!(
                role,
                error = %e,
                "stored tools_allowed is not a JSON array of tool names; the role runs \
                 with no tools until it is updated"
            );
            Some(Vec::new())
        }
    }
}

/// Load a role from the database, falling back to built-in defaults.
pub async fn load_role(name: &str) -> Option<AgentRole> {
    if let Ok(db) = Database::open().await {
        if let Ok(Some(row)) = db.get_role(name).await {
            let tools_allowed = decode_stored_tools_allowed(&row.name, row.tools_allowed);
            return Some(AgentRole {
                name: row.name,
                description: row.description,
                model: row.model,
                provider: row.provider,
                temperature: row.temperature,
                system_instructions: row.system_instructions,
                tools_allowed,
                max_iterations: row.max_iterations.map(|v| v as u32),
            });
        }
    }
    builtin_role(name)
}

/// List all available roles (DB + built-in).
pub async fn list_all_roles() -> Vec<AgentRole> {
    let mut roles = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // DB roles are inserted first so a same-named built-in is skipped below:
    // an operator-defined role must be able to shadow the shipped one.
    if let Ok(db) = Database::open().await {
        if let Ok(db_roles) = db.list_roles().await {
            for row in db_roles {
                seen.insert(row.name.clone());
                let tools_allowed = decode_stored_tools_allowed(&row.name, row.tools_allowed);
                roles.push(AgentRole {
                    name: row.name,
                    description: row.description,
                    model: row.model,
                    provider: row.provider,
                    temperature: row.temperature,
                    system_instructions: row.system_instructions,
                    tools_allowed,
                    max_iterations: row.max_iterations.map(|v| v as u32),
                });
            }
        }
    }

    for (name, ..) in BUILTIN_ROLES {
        if !seen.contains(*name) {
            if let Some(role) = builtin_role(name) {
                roles.push(role);
            }
        }
    }

    roles
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored allowlist that no longer decodes must fail closed. Mapping it
    /// to `None` would hand a locked-down role every tool with no trace; `[]`
    /// is "no tools", which is the safe reading of a list we cannot read.
    #[test]
    fn corrupt_stored_tools_allowed_means_no_tools_not_every_tool() {
        assert_eq!(decode_stored_tools_allowed("r", None), None);
        assert_eq!(
            decode_stored_tools_allowed("r", Some(r#"["fs_read"]"#.into())),
            Some(vec!["fs_read".to_string()])
        );
        assert_eq!(
            decode_stored_tools_allowed("r", Some("[]".into())),
            Some(Vec::new()),
            "an explicit empty list stays empty"
        );
        for corrupt in [
            "not json",
            r#"{"fs_read": true}"#,
            r#"["fs_read", 7]"#,
            "null",
        ] {
            assert_eq!(
                decode_stored_tools_allowed("r", Some(corrupt.into())),
                Some(Vec::new()),
                "{corrupt:?} must decode to an empty allowlist, never to unrestricted"
            );
        }
    }

    #[test]
    fn test_builtin_role_nonexistent() {
        assert!(builtin_role("nonexistent").is_none());
    }

    #[test]
    fn builtin_roles_are_well_formed() {
        // Every role registered in BUILTIN_ROLES must round-trip through
        // builtin_role(), have a temperature in the sampler's valid [0, 2]
        // range, and carry a non-empty tools_allowed list (an empty list would
        // silently block every tool call the sub-agent tries to make).
        for (name, ..) in BUILTIN_ROLES {
            let role = builtin_role(name).unwrap_or_else(|| panic!("{name} should resolve"));
            assert_eq!(
                role.name, *name,
                "builtin_role returned wrong name for {name}"
            );
            let temp = role
                .temperature
                .unwrap_or_else(|| panic!("{name} missing temperature"));
            assert!(
                (0.0..=2.0).contains(&temp),
                "{name} temperature {temp} outside [0, 2]"
            );
            let tools = role
                .tools_allowed
                .as_ref()
                .unwrap_or_else(|| panic!("{name} missing tools_allowed"));
            assert!(!tools.is_empty(), "{name} has empty tools_allowed");
        }
    }
}
