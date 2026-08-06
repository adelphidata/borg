use std::str::FromStr;

use anyhow::Result;
use clap::Subcommand;
use protocols::admin::ScheduledTaskCreate;

use super::{format_ts, short_id, truncate_str, TS_FMT};
use crate::async_bridge::block_on;
use crate::daemon_client::connect_or_spawn;

#[derive(Subcommand)]
pub(crate) enum TasksAction {
    /// List all scheduled tasks
    List,
    /// Create a new scheduled task
    Create {
        /// Task name
        #[arg(long, short)]
        name: String,
        /// Prompt to send to the agent
        #[arg(long, short)]
        prompt: String,
        /// Schedule expression (cron or interval)
        #[arg(long, short)]
        schedule: String,
        /// Schedule type: cron, interval, or once
        #[arg(long, short = 't', default_value = "cron")]
        r#type: String,
        /// Max retry attempts for transient failures (default: 3)
        #[arg(long)]
        max_retries: Option<i32>,
        /// Timeout in seconds (default: 300)
        #[arg(long)]
        timeout: Option<u64>,
        /// Delivery channel for results (telegram, slack, discord)
        #[arg(long)]
        delivery_channel: Option<String>,
        /// Delivery target (chat_id or channel_id)
        #[arg(long)]
        delivery_target: Option<String>,
    },
    /// Delete a scheduled task
    Delete {
        /// Task ID (or prefix)
        id: String,
    },
    /// Pause a scheduled task
    Pause {
        /// Task ID (or prefix)
        id: String,
    },
    /// Resume a paused task
    Resume {
        /// Task ID (or prefix)
        id: String,
    },
    /// Trigger a task to run immediately
    Run {
        /// Task ID (or prefix)
        id: String,
    },
    /// Show execution history for a task
    Runs {
        /// Task ID (or prefix)
        id: String,
        /// Number of runs to show
        #[arg(long, short, default_value_t = 10)]
        count: usize,
    },
    /// Show detailed task status
    Status {
        /// Task ID (or prefix)
        id: String,
    },
    /// Pin the model a task runs on (or clear the pin)
    ///
    /// Scheduled runs otherwise follow `tasks.model`, and failing that the
    /// global `llm.model` — so changing the model you chat with also changes
    /// what your scheduled fleet costs. This is user-owned: the agent cannot
    /// set it.
    Model {
        /// Task ID (or prefix)
        id: String,
        /// Model id to pin. Omit with --clear to remove the pin.
        model: Option<String>,
        /// Provider for the pinned model (defaults to the configured provider)
        #[arg(long)]
        provider: Option<String>,
        /// Remove the pin so the task follows tasks.model / llm.model again
        #[arg(long)]
        clear: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum CronAction {
    /// List all cron jobs
    List,
    /// Add a cron job. Use combined format: "*/5 * * * * echo hello"
    /// or separate flags: -s "*/5 * * * *" -c "echo hello"
    Add {
        /// Combined crontab line: "*/5 * * * * command args..."
        line: Option<String>,
        /// Cron schedule (5-field Linux format, e.g. "*/5 * * * *")
        #[arg(long, short)]
        schedule: Option<String>,
        /// Shell command to execute
        #[arg(long, short)]
        command: Option<String>,
        /// Job name (auto-generated from command if omitted)
        #[arg(long, short)]
        name: Option<String>,
        /// Timeout in seconds (default: 300)
        #[arg(long)]
        timeout: Option<u64>,
        /// Delivery channel for output (telegram, slack, discord)
        #[arg(long)]
        delivery_channel: Option<String>,
        /// Delivery target (chat_id or channel_id)
        #[arg(long)]
        delivery_target: Option<String>,
    },
    /// Remove a cron job
    Remove {
        /// Job ID (or prefix)
        id: String,
    },
    /// Pause a cron job
    Pause {
        /// Job ID (or prefix)
        id: String,
    },
    /// Resume a paused cron job
    Resume {
        /// Job ID (or prefix)
        id: String,
    },
    /// Trigger a cron job to run immediately
    Run {
        /// Job ID (or prefix)
        id: String,
    },
    /// Show execution history for a cron job
    Runs {
        /// Job ID (or prefix)
        id: String,
        /// Number of runs to show
        #[arg(long, short, default_value_t = 10)]
        count: usize,
    },
}

/// Dispatch for `borg tasks ...`.
pub(crate) fn dispatch_tasks(action: Option<TasksAction>) -> Result<()> {
    match action {
        Some(TasksAction::List) | None => run_tasks_list(),
        Some(TasksAction::Create {
            name,
            prompt,
            schedule,
            r#type,
            max_retries,
            timeout,
            delivery_channel,
            delivery_target,
        }) => run_tasks_create(
            &name,
            &prompt,
            &schedule,
            &r#type,
            max_retries,
            timeout.map(|s| s as i64 * 1000),
            delivery_channel.as_deref(),
            delivery_target.as_deref(),
        ),
        Some(TasksAction::Delete { id }) => run_tasks_delete(&id),
        Some(TasksAction::Pause { id }) => run_tasks_pause(&id),
        Some(TasksAction::Resume { id }) => run_tasks_resume(&id),
        Some(TasksAction::Run { id }) => run_tasks_run(&id),
        Some(TasksAction::Runs { id, count }) => run_tasks_runs(&id, count),
        Some(TasksAction::Status { id }) => run_tasks_status(&id),
        Some(TasksAction::Model {
            id,
            model,
            provider,
            clear,
        }) => run_tasks_model(&id, model.as_deref(), provider.as_deref(), clear),
    }
}

/// `borg tasks model <id> [<model>] [--provider P] [--clear]`.
pub(crate) fn run_tasks_model(
    id: &str,
    model: Option<&str>,
    provider: Option<&str>,
    clear: bool,
) -> Result<()> {
    if clear && (model.is_some() || provider.is_some()) {
        anyhow::bail!("pass either a model id (with optional --provider) or --clear, not both");
    }
    if !clear && model.is_none() {
        anyhow::bail!("provide a model id to pin, or --clear to remove the pin");
    }
    // Reject an unknown provider here rather than persisting a typo that only
    // surfaces as a failed run at 3am.
    if let Some(p) = provider {
        use cortex::llm::provider::Provider;
        if Provider::from_str(p).is_err() {
            anyhow::bail!(
                "unknown provider '{p}' — expected one of: {}",
                Provider::ALL
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    block_on(async {
        let db = cortex::db::Database::open().await?;
        // Accept an id prefix like every other `borg tasks` subcommand.
        let full_id = match resolve_task_id_prefix(&db, id).await? {
            Some(full) => full,
            None => {
                println!("Task not found: {id}");
                return Ok(());
            }
        };
        // The guards above make `clear` equivalent to "both are None".
        db.set_task_model_pin(&full_id, model, provider).await?;
        match model {
            Some(m) => {
                let via = provider.map(|p| format!(" via {p}")).unwrap_or_default();
                println!("Task {} pinned to {m}{via}", short_id(&full_id));
            }
            None => println!(
                "Task {} pin cleared — it now follows tasks.model / llm.model",
                short_id(&full_id)
            ),
        }
        Ok(())
    })
}

/// Resolve a task id prefix to the full id, or `None` when nothing matches.
async fn resolve_task_id_prefix(db: &cortex::db::Database, prefix: &str) -> Result<Option<String>> {
    // `list_tasks` is unpaged, so filtering it can't miss a match.
    let ids = db.list_tasks().await?.into_iter().map(|t| t.id).collect();
    super::resolve_unique_prefix("tasks", prefix, ids)
}

/// Dispatch for `borg cron ...`.
pub(crate) fn dispatch_cron(action: Option<CronAction>) -> Result<()> {
    match action {
        Some(CronAction::List) | None => run_cron_list(),
        Some(CronAction::Add {
            line,
            schedule,
            command,
            name,
            timeout,
            delivery_channel,
            delivery_target,
        }) => run_cron_add(
            line.as_deref(),
            schedule.as_deref(),
            command.as_deref(),
            name.as_deref(),
            timeout.map(|s| s as i64 * 1000),
            delivery_channel.as_deref(),
            delivery_target.as_deref(),
        ),
        Some(CronAction::Remove { id }) => run_cron_mutate(&id, "delete"),
        Some(CronAction::Pause { id }) => run_cron_mutate(&id, "pause"),
        Some(CronAction::Resume { id }) => run_cron_mutate(&id, "resume"),
        Some(CronAction::Run { id }) => run_cron_mutate(&id, "run"),
        Some(CronAction::Runs { id, count }) => run_tasks_runs(&id, count),
    }
}

pub(crate) fn run_tasks_list() -> Result<()> {
    let tasks = block_on(async {
        let mut client = connect_or_spawn().await?;
        let resp = client.list_scheduled_tasks().await?;
        Ok::<_, anyhow::Error>(resp.tasks)
    })?;

    if tasks.is_empty() {
        println!("No scheduled tasks.");
    } else {
        println!(
            "{:8}  {:20}  {:8}  {:8}  {:20}  NEXT RUN",
            "ID", "NAME", "TYPE", "STATUS", "SCHEDULE"
        );
        for task in &tasks {
            let next_run = if task.next_run_unix > 0 {
                format_ts(task.next_run_unix, "%Y-%m-%d %H:%M:%S")
            } else {
                "-".to_string()
            };
            println!(
                "{:8}  {:20}  {:8}  {:8}  {:20}  {}",
                short_id(&task.id),
                truncate_str(&task.name, 20),
                task.schedule_type,
                task.status,
                truncate_str(&task.schedule_expr, 20),
                next_run,
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_tasks_create(
    name: &str,
    prompt: &str,
    schedule: &str,
    schedule_type: &str,
    max_retries: Option<i32>,
    timeout_ms: Option<i64>,
    delivery_channel: Option<&str>,
    delivery_target: Option<&str>,
) -> Result<()> {
    // Validate locally so we can fail fast with a useful error before the
    // round-trip. The daemon revalidates on the way in.
    cortex::tasks::validate_schedule(schedule_type, schedule)?;
    let tz = "local";

    let spec = ScheduledTaskCreate {
        name: name.to_string(),
        prompt: prompt.to_string(),
        schedule_type: schedule_type.to_string(),
        schedule_expr: schedule.to_string(),
        timezone: tz.to_string(),
        task_type: "prompt".to_string(),
        delivery_channel: delivery_channel.unwrap_or("").to_string(),
        delivery_target: delivery_target.unwrap_or("").to_string(),
        next_run_unix: 0,
        // Sentinel: <0 means "leave to DB default" so the daemon's
        // ScheduledTaskCreate field maps cleanly to the underlying
        // NewTask.max_retries: Option<i32> on the daemon side.
        max_retries: max_retries.unwrap_or(-1),
        timeout_ms: timeout_ms.unwrap_or(0),
        allowed_tools: String::new(),
        pre_run_command: String::new(),
        pre_run_cwd: String::new(),
        pre_run_timeout_ms: 0,
    };

    let task = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.create_scheduled_task(spec).await
    })?;

    println!("Created task {} ({})", short_id(&task.id), task.name);
    Ok(())
}

pub(crate) fn run_tasks_run(id: &str) -> Result<()> {
    let result = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.run_scheduled_task_now(id).await
    });
    match result {
        Ok(()) => println!("Task {} queued for immediate execution.", short_id(id)),
        Err(e) => {
            // NotFound from the daemon surfaces as an anyhow error wrapping the
            // tonic Status; print a friendly message rather than failing.
            if format!("{e:?}").contains("NotFound") {
                println!("Task not found: {id}");
            } else {
                return Err(e);
            }
        }
    }
    Ok(())
}

pub(crate) fn run_tasks_runs(id: &str, count: usize) -> Result<()> {
    let runs = block_on(async {
        let mut client = connect_or_spawn().await?;
        let limit = u32::try_from(count).unwrap_or(u32::MAX);
        let resp = client.get_scheduled_task_runs(id, limit).await?;
        Ok::<_, anyhow::Error>(resp.runs)
    })?;
    if runs.is_empty() {
        println!("No runs recorded for task {}", short_id(id));
        return Ok(());
    }
    println!("{:<20} {:<8} {:<10} Details", "Time", "Status", "Duration");
    println!("{}", "-".repeat(70));
    for run in &runs {
        let when = format_ts(run.started_at_unix, TS_FMT);
        let status = cortex::tasks::format_run_status(&run.status);
        let duration = format!("{}ms", run.duration_ms);
        let details_src = if !run.error.is_empty() {
            run.error.as_str()
        } else {
            run.result.as_str()
        };
        let details: String = details_src.chars().take(40).collect();
        println!("{when:<20} {status:<8} {duration:<10} {details}");
    }
    Ok(())
}

pub(crate) fn run_tasks_status(id: &str) -> Result<()> {
    let detail = match block_on(async {
        let mut client = connect_or_spawn().await?;
        client.get_scheduled_task_detail(id).await
    }) {
        Ok(d) => d,
        Err(e) if format!("{e:?}").contains("NotFound") => {
            println!("Task not found: {id}");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    // Render directly from the proto so this command doesn't depend on any
    // core DB types. Mirrors the layout that `cortex::tasks::format_task`
    // produces for the prompt/command head.
    let task = detail
        .task
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("ScheduledTaskDetail.task missing"))?;
    let next_str = if task.next_run_unix > 0 {
        format_ts(task.next_run_unix, "%Y-%m-%d %H:%M UTC")
    } else {
        "—".to_string()
    };
    let payload_label = if task.task_type == "command" {
        "Command"
    } else {
        "Prompt"
    };
    println!(
        "  {} [{}] ({})\n    Schedule: {} {}\n    Next run: {}\n    {}: {}",
        task.name,
        task.status,
        short_id(&task.id),
        task.schedule_type,
        task.schedule_expr,
        next_str,
        payload_label,
        truncate_str(&task.prompt, 80),
    );
    println!("    Max retries: {}", detail.max_retries);
    println!("    Timeout: {}ms", detail.timeout_ms);
    if !task.delivery_channel.is_empty() {
        let target = if task.delivery_target.is_empty() {
            "?"
        } else {
            task.delivery_target.as_str()
        };
        println!("    Delivery: {} -> {}", task.delivery_channel, target);
    }
    // Outside the retry branch on purpose: a policy refusal or a delivery
    // failure leaves `retry_count` at 0, and nesting this made those errors
    // invisible in `borg tasks status` — the one place an operator looks.
    if !detail.last_error.is_empty() {
        let err = &detail.last_error;
        println!("    Last error: {}", &err[..err.len().min(100)]);
    }
    if detail.retry_count > 0 {
        println!(
            "    Retry state: attempt {}/{}",
            detail.retry_count, detail.max_retries
        );
        if detail.retry_after_unix > 0 {
            let when = format_ts(detail.retry_after_unix, "%Y-%m-%d %H:%M UTC");
            println!("    Next retry: {when}");
        }
    }
    if detail.has_last_run {
        if let Some(run) = detail.last_run.as_ref() {
            let when = format_ts(run.started_at_unix, "%Y-%m-%d %H:%M UTC");
            println!(
                "    Last run: {} at {when} ({}ms)",
                run.status, run.duration_ms
            );
        }
    }
    Ok(())
}

pub(crate) fn run_tasks_delete(id: &str) -> Result<()> {
    let result = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.delete_scheduled_task(id).await
    });
    match result {
        Ok(()) => println!("Deleted task {}", short_id(id)),
        Err(e) if format!("{e:?}").contains("NotFound") => println!("Task not found: {id}"),
        Err(e) => return Err(e),
    }
    Ok(())
}

pub(crate) fn run_tasks_pause(id: &str) -> Result<()> {
    pause_or_resume(id, /* pause = */ true)
}

pub(crate) fn run_tasks_resume(id: &str) -> Result<()> {
    pause_or_resume(id, /* pause = */ false)
}

fn pause_or_resume(id: &str, pause: bool) -> Result<()> {
    let result = block_on(async {
        let mut client = connect_or_spawn().await?;
        if pause {
            client.pause_scheduled_task(id).await
        } else {
            client.resume_scheduled_task(id).await
        }
    });
    let status = if pause { "paused" } else { "active" };
    match result {
        Ok(()) => println!("Task {} status: {status}", short_id(id)),
        Err(e) if format!("{e:?}").contains("NotFound") => println!("Task not found: {id}"),
        Err(e) => return Err(e),
    }
    Ok(())
}

pub(crate) fn run_cron_list() -> Result<()> {
    let tasks = block_on(async {
        let mut client = connect_or_spawn().await?;
        let resp = client.list_scheduled_tasks().await?;
        Ok::<_, anyhow::Error>(resp.tasks)
    })?;
    let cron_jobs: Vec<_> = tasks.iter().filter(|t| t.task_type == "command").collect();
    if cron_jobs.is_empty() {
        println!("No cron jobs. Use `borg cron add` to create one.");
        return Ok(());
    }
    println!(
        "{:<10} {:<8} {:<20} {:<30} NEXT RUN",
        "ID", "STATUS", "SCHEDULE", "COMMAND"
    );
    println!("{}", "-".repeat(90));
    for job in &cron_jobs {
        let next = if job.next_run_unix > 0 {
            format_ts(job.next_run_unix, TS_FMT)
        } else {
            "—".to_string()
        };
        let sched_display = display_cron_5field(&job.schedule_expr);
        println!(
            "{:<10} {:<8} {:<20} {:<30} {}",
            short_id(&job.id),
            job.status,
            sched_display,
            truncate_str(&job.prompt, 28),
            next,
        );
    }
    Ok(())
}

/// Convert a 7-field cron expression back to 5-field Linux format for display.
fn display_cron_5field(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() == 7 {
        fields[1..6].join(" ")
    } else {
        expr.to_string()
    }
}

pub(crate) fn run_cron_add(
    line: Option<&str>,
    schedule: Option<&str>,
    command: Option<&str>,
    name: Option<&str>,
    timeout_ms: Option<i64>,
    delivery_channel: Option<&str>,
    delivery_target: Option<&str>,
) -> Result<()> {
    let (cron_7, cmd) = if let Some(line) = line {
        cortex::tasks::parse_cron_line(line)?
    } else {
        match (schedule, command) {
            (Some(sched), Some(cmd)) => {
                let cron_7 = cortex::tasks::convert_5_to_7_field(sched);
                cortex::tasks::validate_schedule("cron", &cron_7)?;
                (cron_7, cmd.to_string())
            }
            _ => anyhow::bail!(
                "Provide either a combined crontab line or both --schedule and --command.\n\
                 Examples:\n  borg cron add \"*/5 * * * * echo hello\"\n  \
                 borg cron add -s \"*/5 * * * *\" -c \"echo hello\""
            ),
        }
    };

    let job_name = name
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| {
            let short_cmd: String = cmd.chars().take(30).collect();
            format!("cron: {short_cmd}")
        });

    let next_run = cortex::tasks::calculate_next_run("cron", &cron_7, "local")?;

    let spec = ScheduledTaskCreate {
        name: job_name.clone(),
        prompt: cmd.clone(),
        schedule_type: "cron".to_string(),
        schedule_expr: cron_7.clone(),
        timezone: chrono::Local::now().offset().to_string(),
        task_type: "command".to_string(),
        delivery_channel: delivery_channel.unwrap_or("").to_string(),
        delivery_target: delivery_target.unwrap_or("").to_string(),
        next_run_unix: next_run.unwrap_or(0),
        // Cron jobs explicitly opt out of retries (the original direct-DB
        // path passed Some(0)). Use 0, not -1 sentinel.
        max_retries: 0,
        timeout_ms: timeout_ms.unwrap_or(0),
        allowed_tools: String::new(),
        pre_run_command: String::new(),
        pre_run_cwd: String::new(),
        pre_run_timeout_ms: 0,
    };

    let task = block_on(async {
        let mut client = connect_or_spawn().await?;
        client.create_scheduled_task(spec).await
    })?;

    let next_str = if task.next_run_unix > 0 {
        format_ts(task.next_run_unix, TS_FMT)
    } else {
        "?".to_string()
    };
    println!(
        "Created cron job {} ({})\n  Schedule: {}\n  Command: {}\n  Next run: {}",
        short_id(&task.id),
        job_name,
        display_cron_5field(&cron_7),
        cmd,
        next_str,
    );
    Ok(())
}

/// Type-guarded cron job mutation. Verifies the task is a command-type before
/// mutating so a stray `borg cron remove <prompt-task-id>` doesn't silently
/// nuke a prompt task — preserves the same guard the direct-DB version had.
pub(crate) fn run_cron_mutate(id: &str, action: &str) -> Result<()> {
    let outcome = block_on(async {
        let mut client = connect_or_spawn().await?;
        let detail = match client.get_scheduled_task_detail(id).await {
            Ok(d) => d,
            Err(e) if format!("{e:?}").contains("NotFound") => return Ok(MutateOutcome::NotFound),
            Err(e) => return Err(e),
        };
        let Some(task) = detail.task.as_ref() else {
            return Err(anyhow::anyhow!("ScheduledTaskDetail.task missing"));
        };
        if task.task_type != "command" {
            return Ok(MutateOutcome::WrongType);
        }
        match action {
            "delete" => client.delete_scheduled_task(id).await?,
            "pause" => client.pause_scheduled_task(id).await?,
            "resume" => client.resume_scheduled_task(id).await?,
            "run" => client.run_scheduled_task_now(id).await?,
            other => unreachable!("unknown cron action: {other}"),
        }
        Ok(MutateOutcome::Ok)
    })?;

    match (outcome, action) {
        (MutateOutcome::Ok, "delete") => println!("Deleted cron job {}", short_id(id)),
        (MutateOutcome::Ok, "pause") => println!("Cron job {} paused", short_id(id)),
        (MutateOutcome::Ok, "resume") => println!("Cron job {} resumed", short_id(id)),
        (MutateOutcome::Ok, "run") => {
            println!("Cron job {} queued for immediate execution", short_id(id))
        }
        (MutateOutcome::Ok, _) => unreachable!(),
        (MutateOutcome::WrongType, _) => {
            println!("Not a cron job: {id} (use `borg tasks` for prompt tasks)")
        }
        (MutateOutcome::NotFound, _) => println!("Cron job not found: {id}"),
    }
    Ok(())
}

enum MutateOutcome {
    Ok,
    WrongType,
    NotFound,
}
