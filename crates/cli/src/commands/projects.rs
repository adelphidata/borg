use anyhow::Result;
use uuid::Uuid;

use super::{format_ts, short_id, truncate_str};

pub(crate) fn run_projects_list(status_filter: Option<&str>) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let projects = db.list_projects(status_filter)?;

    if projects.is_empty() {
        println!("No projects.");
    } else {
        println!(
            "{:8}  {:30}  {:10}  {:19}  DESCRIPTION",
            "ID", "NAME", "STATUS", "CREATED"
        );
        for p in &projects {
            let created = format_ts(p.created_at, "%Y-%m-%d %H:%M:%S");
            let desc = truncate_str(&p.description, 40);
            println!(
                "{:8}  {:30}  {:10}  {:19}  {}",
                short_id(&p.id),
                truncate_str(&p.name, 30),
                p.status,
                created,
                desc,
            );
        }
    }
    Ok(())
}

pub(crate) fn run_projects_create(name: &str, description: Option<&str>) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    let db = borg_core::db::Database::open()?;
    db.create_project(&id, name, description.unwrap_or(""))?;
    println!("Created project {} ({})", short_id(&id), name);
    Ok(())
}

pub(crate) fn run_projects_get(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    match db.get_project(id)? {
        Some(p) => {
            println!("Project: {}", p.name);
            println!("  ID:          {}", p.id);
            println!("  Status:      {}", p.status);
            println!(
                "  Description: {}",
                if p.description.is_empty() {
                    "(none)"
                } else {
                    &p.description
                }
            );
            println!(
                "  Created:     {}",
                format_ts(p.created_at, "%Y-%m-%d %H:%M:%S")
            );
            println!(
                "  Updated:     {}",
                format_ts(p.updated_at, "%Y-%m-%d %H:%M:%S")
            );

            match db.list_workflows_by_project(&p.id) {
                Ok(wfs) if wfs.is_empty() => println!("  Workflows:   none"),
                Ok(wfs) => {
                    println!("  Workflows ({}):", wfs.len());
                    for wf in &wfs {
                        println!("    [{}] {} ({})", wf.status, wf.title, short_id(&wf.id),);
                    }
                }
                Err(e) => println!("  Workflows:   error ({e})"),
            }
        }
        None => println!("Project not found: {id}"),
    }
    Ok(())
}

pub(crate) fn run_projects_update(
    id: &str,
    name: Option<&str>,
    description: Option<&str>,
    status: Option<&str>,
) -> Result<()> {
    if name.is_none() && description.is_none() && status.is_none() {
        anyhow::bail!("Nothing to update. Provide --name, --description, or --status.");
    }
    let db = borg_core::db::Database::open()?;
    if db.update_project(id, name, description, status)? {
        println!("Updated project {}", short_id(id));
    } else {
        println!("Project not found: {id}");
    }
    Ok(())
}

pub(crate) fn run_projects_archive(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.archive_project(id)? {
        println!("Archived project {}", short_id(id));
    } else {
        println!("Project not found or already archived: {id}");
    }
    Ok(())
}

pub(crate) fn run_projects_delete(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.delete_project(id)? {
        println!("Deleted project {}", short_id(id));
    } else {
        println!("Project not found: {id}");
    }
    Ok(())
}
