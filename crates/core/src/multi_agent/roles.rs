use super::AgentRole;
use crate::db::Database;

/// Built-in role definitions.
pub const BUILTIN_ROLES: &[(&str, &str, f32, &[&str])] = &[
    (
        "researcher",
        "Information gathering and analysis. Use this role for tasks that require searching, reading, and synthesizing information.",
        0.3,
        &["run_shell", "web_fetch", "web_search", "read_memory", "write_memory"],
    ),
    (
        "coder",
        "Code writing and modification. Use this role for tasks that require creating or modifying code files.",
        0.2,
        &["run_shell", "apply_patch", "read_memory"],
    ),
    (
        "writer",
        "Documentation and content writing. Use this role for tasks that require writing documentation, notes, or creative content.",
        0.7,
        &["run_shell", "apply_patch", "read_memory", "write_memory", "web_search"],
    ),
];

/// Build an AgentRole from built-in defaults.
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

/// Seed the built-in roles into the database if they don't already exist.
pub fn seed_builtin_roles(db: &Database) -> anyhow::Result<()> {
    for (name, desc, temp, tools) in BUILTIN_ROLES {
        if db.get_role(name)?.is_none() {
            let tools_json =
                serde_json::to_string(&tools.iter().collect::<Vec<_>>()).unwrap_or_default();
            db.insert_role(
                name,
                desc,
                None,
                None,
                Some(*temp),
                None,
                Some(&tools_json),
                None,
                true,
            )?;
        }
    }
    Ok(())
}

/// Load a role from the database, falling back to built-in defaults.
pub fn load_role(name: &str) -> Option<AgentRole> {
    // Try DB first
    if let Ok(db) = Database::open() {
        if let Ok(Some(row)) = db.get_role(name) {
            let tools_allowed = row
                .tools_allowed
                .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok());
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
    // Fallback to built-in
    builtin_role(name)
}

/// List all available roles (DB + built-in).
pub fn list_all_roles() -> Vec<AgentRole> {
    let mut roles = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // DB roles take priority
    if let Ok(db) = Database::open() {
        if let Ok(db_roles) = db.list_roles() {
            for row in db_roles {
                seen.insert(row.name.clone());
                let tools_allowed = row
                    .tools_allowed
                    .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok());
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

    // Add any built-in roles not already in DB
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
