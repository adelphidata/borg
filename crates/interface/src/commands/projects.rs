use anyhow::Result;
use clap::Subcommand;

use super::{format_ts, short_id, truncate_str};
use crate::daemon_client::connect_or_spawn;

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

/// Block on a future from a sync context. Reuses the ambient tokio runtime
/// (`main()` builds one in `crates/cli/src/main.rs`).
fn block_on<F: std::future::Future<Output = R>, R>(fut: F) -> R {
    tokio::runtime::Handle::current().block_on(fut)
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
    let resp = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.list_projects().await
    })?;
    let mut projects = resp.projects;
    // Apply optional status filter client-side (admin RPC currently returns all).
    if let Some(want) = status_filter {
        projects.retain(|p| p.status == want);
    }

    if projects.is_empty() {
        println!("No projects.");
    } else {
        println!(
            "{:8}  {:30}  {:10}  {:19}  DESCRIPTION",
            "ID", "NAME", "STATUS", "CREATED"
        );
        for p in &projects {
            let created = format_ts(p.created_at as i64, "%Y-%m-%d %H:%M:%S");
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
    let project = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.create_project(name, description.unwrap_or("")).await
    })?;
    println!("Created project {} ({})", short_id(&project.id), name);
    Ok(())
}

pub(crate) fn run_projects_get(id: &str) -> Result<()> {
    let (matched, workflows) = block_on(async {
        let mut client = connect_or_spawn().await?;
        let list = client.list_projects().await?;
        // Match by exact id or by short-id prefix to preserve prior UX.
        let matched = list
            .projects
            .into_iter()
            .find(|p| p.id == id || p.id.starts_with(id));
        let workflows = match &matched {
            Some(p) => match client.list_workflows_by_project(&p.id).await {
                Ok(w) => Some(w.workflows),
                Err(e) => {
                    tracing::warn!("list_workflows_by_project failed: {e}");
                    None
                }
            },
            None => None,
        };
        Ok::<_, anyhow::Error>((matched, workflows))
    })?;

    match matched {
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
                format_ts(p.created_at as i64, "%Y-%m-%d %H:%M:%S")
            );
            println!(
                "  Updated:     {}",
                format_ts(p.updated_at as i64, "%Y-%m-%d %H:%M:%S")
            );

            match workflows {
                Some(wfs) if wfs.is_empty() => println!("  Workflows:   none"),
                Some(wfs) => {
                    println!("  Workflows ({}):", wfs.len());
                    for wf in &wfs {
                        println!("    [{}] {} ({})", wf.status, wf.title, short_id(&wf.id),);
                    }
                }
                None => println!("  Workflows:   error"),
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
    let result = block_on(async {
        let mut client = connect_or_spawn().await?;
        // ProjectUpdate: empty string = leave unchanged, matching daemon proto.
        client
            .update_project(
                id,
                name.unwrap_or(""),
                description.unwrap_or(""),
                status.unwrap_or(""),
            )
            .await
    });
    match result {
        Ok(_) => {
            println!("Updated project {}", short_id(id));
            Ok(())
        }
        Err(e) => {
            if e.to_string().contains("not found") {
                println!("Project not found: {id}");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

pub(crate) fn run_projects_archive(id: &str) -> Result<()> {
    let result = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.archive_project(id).await
    });
    match result {
        Ok(()) => {
            println!("Archived project {}", short_id(id));
            Ok(())
        }
        Err(e) => {
            if e.to_string().contains("not found") {
                println!("Project not found or already archived: {id}");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

pub(crate) fn run_projects_delete(id: &str) -> Result<()> {
    let result = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.delete_project(id).await
    });
    match result {
        Ok(()) => {
            println!("Deleted project {}", short_id(id));
            Ok(())
        }
        Err(e) => {
            if e.to_string().contains("not found") {
                println!("Project not found: {id}");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}
