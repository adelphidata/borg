use anyhow::Result;
use clap::Subcommand;
use uuid::Uuid;

use super::{format_ts, short_id, truncate_str};

#[derive(Subcommand)]
pub(crate) enum ProjectsAction {
    /// List all projects
    List {
        /// Filter by status (active, archived)
        #[arg(long, short)]
        status: Option<String>,
    },
    /// Create a new project
    Create {
        /// Project name
        #[arg(long, short)]
        name: String,
        /// Project description
        #[arg(long, short)]
        description: Option<String>,
    },
    /// Show project details and associated workflows
    Get {
        /// Project ID (or prefix)
        id: String,
    },
    /// Update a project's fields
    Update {
        /// Project ID (or prefix)
        id: String,
        /// New project name
        #[arg(long, short)]
        name: Option<String>,
        /// New project description
        #[arg(long, short)]
        description: Option<String>,
        /// New status (active, archived)
        #[arg(long, short)]
        status: Option<String>,
    },
    /// Archive a project
    Archive {
        /// Project ID (or prefix)
        id: String,
    },
    /// Delete a project
    Delete {
        /// Project ID (or prefix)
        id: String,
    },
}

/// Dispatch for `borg projects ...`.
pub(crate) fn dispatch_projects(action: Option<ProjectsAction>) -> Result<()> {
    match action {
        Some(ProjectsAction::List { status }) => run_projects_list(status.as_deref()),
        None => run_projects_list(None),
        Some(ProjectsAction::Create { name, description }) => {
            run_projects_create(&name, description.as_deref())
        }
        Some(ProjectsAction::Get { id }) => run_projects_get(&id),
        Some(ProjectsAction::Update {
            id,
            name,
            description,
            status,
        }) => run_projects_update(
            &id,
            name.as_deref(),
            description.as_deref(),
            status.as_deref(),
        ),
        Some(ProjectsAction::Archive { id }) => run_projects_archive(&id),
        Some(ProjectsAction::Delete { id }) => run_projects_delete(&id),
    }
}

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
