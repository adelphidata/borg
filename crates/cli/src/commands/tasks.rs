use anyhow::Result;
use clap::Subcommand;
use uuid::Uuid;

use super::{format_ts, short_id, truncate_str};

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
        Some(TasksAction::Pause { id }) => run_tasks_update_status(&id, "paused"),
        Some(TasksAction::Resume { id }) => run_tasks_update_status(&id, "active"),
        Some(TasksAction::Run { id }) => run_tasks_run(&id),
        Some(TasksAction::Runs { id, count }) => run_tasks_runs(&id, count),
        Some(TasksAction::Status { id }) => run_tasks_status(&id),
    }
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
    let db = borg_core::db::Database::open()?;
    let tasks = db.list_tasks()?;

    if tasks.is_empty() {
        println!("No scheduled tasks.");
    } else {
        println!(
            "{:8}  {:20}  {:8}  {:8}  {:20}  NEXT RUN",
            "ID", "NAME", "TYPE", "STATUS", "SCHEDULE"
        );
        for task in &tasks {
            let next_run = task
                .next_run
                .map(|ts| format_ts(ts, "%Y-%m-%d %H:%M:%S"))
                .unwrap_or_else(|| "-".to_string());
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
    borg_core::tasks::validate_schedule(schedule_type, schedule)?;
    let next_run = borg_core::tasks::calculate_next_run(schedule_type, schedule)?;
    let id = Uuid::new_v4().to_string();
    let tz = chrono::Local::now().offset().to_string();

    let db = borg_core::db::Database::open()?;
    db.create_task(&borg_core::db::NewTask {
        id: &id,
        name,
        prompt,
        schedule_type,
        schedule_expr: schedule,
        timezone: &tz,
        next_run,
        max_retries,
        timeout_ms,
        delivery_channel,
        delivery_target,
        allowed_tools: None,
        task_type: "prompt",
    })?;

    println!("Created task {} ({})", short_id(&id), name);
    Ok(())
}

pub(crate) fn run_tasks_run(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    match db.get_task_by_id(id)? {
        Some(_task) => {
            let now = chrono::Utc::now().timestamp();
            db.update_task_next_run(id, Some(now))?;
            db.clear_task_retry(id)?;
            println!("Task {} queued for immediate execution.", short_id(id));
        }
        None => println!("Task not found: {id}"),
    }
    Ok(())
}

pub(crate) fn run_tasks_runs(id: &str, count: usize) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let runs = db.task_run_history(id, count)?;
    if runs.is_empty() {
        println!("No runs recorded for task {}", short_id(id));
        return Ok(());
    }
    println!("{:<20} {:<8} {:<10} Details", "Time", "Status", "Duration");
    println!("{}", "-".repeat(70));
    for run in &runs {
        let when = format_ts(run.started_at, "%Y-%m-%d %H:%M");
        let status = borg_core::tasks::format_run_status(&run.status);
        let duration = format!("{}ms", run.duration_ms);
        let details = run
            .error
            .as_deref()
            .or(run.result.as_deref())
            .unwrap_or("")
            .chars()
            .take(40)
            .collect::<String>();
        println!("{when:<20} {status:<8} {duration:<10} {details}");
    }
    Ok(())
}

pub(crate) fn run_tasks_status(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    match db.get_task_by_id(id)? {
        Some(task) => {
            println!("{}", borg_core::tasks::format_task(&task));
            println!("    Max retries: {}", task.max_retries);
            println!("    Timeout: {}ms", task.timeout_ms);
            if let Some(ref ch) = task.delivery_channel {
                println!(
                    "    Delivery: {} -> {}",
                    ch,
                    task.delivery_target.as_deref().unwrap_or("?")
                );
            }
            if task.retry_count > 0 {
                println!(
                    "    Retry state: attempt {}/{}",
                    task.retry_count, task.max_retries
                );
                if let Some(ref err) = task.last_error {
                    println!("    Last error: {}", &err[..err.len().min(100)]);
                }
                if let Some(retry_at) = task.retry_after {
                    let when = format_ts(retry_at, "%Y-%m-%d %H:%M UTC");
                    println!("    Next retry: {when}");
                }
            }
            if let Ok(Some(run)) = db.last_task_run(id) {
                let when = format_ts(run.started_at, "%Y-%m-%d %H:%M UTC");
                println!(
                    "    Last run: {} at {when} ({}ms)",
                    run.status, run.duration_ms
                );
            }
        }
        None => println!("Task not found: {id}"),
    }
    Ok(())
}

pub(crate) fn run_tasks_delete(id: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.delete_task(id)? {
        println!("Deleted task {}", short_id(id));
    } else {
        println!("Task not found: {id}");
    }
    Ok(())
}

pub(crate) fn run_tasks_update_status(id: &str, status: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    if db.update_task_status(id, status)? {
        println!("Task {} status: {status}", short_id(id));
    } else {
        println!("Task not found: {id}");
    }
    Ok(())
}

pub(crate) fn run_cron_list() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let tasks = db.list_tasks()?;
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
        let next = job
            .next_run
            .map(|ts| format_ts(ts, "%Y-%m-%d %H:%M"))
            .unwrap_or_else(|| "—".to_string());
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
        borg_core::tasks::parse_cron_line(line)?
    } else {
        match (schedule, command) {
            (Some(sched), Some(cmd)) => {
                let cron_7 = borg_core::tasks::convert_5_to_7_field(sched);
                borg_core::tasks::validate_schedule("cron", &cron_7)?;
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

    let id = uuid::Uuid::new_v4().to_string();
    let next_run = borg_core::tasks::calculate_next_run("cron", &cron_7)?;

    let db = borg_core::db::Database::open()?;
    db.create_task(&borg_core::db::NewTask {
        id: &id,
        name: &job_name,
        prompt: &cmd,
        schedule_type: "cron",
        schedule_expr: &cron_7,
        timezone: &chrono::Local::now().offset().to_string(),
        next_run,
        max_retries: Some(0),
        timeout_ms,
        delivery_channel,
        delivery_target,
        allowed_tools: None,
        task_type: "command",
    })?;

    let next_str = next_run
        .map(|ts| format_ts(ts, "%Y-%m-%d %H:%M"))
        .unwrap_or_else(|| "?".to_string());
    println!(
        "Created cron job {} ({})\n  Schedule: {}\n  Command: {}\n  Next run: {}",
        short_id(&id),
        job_name,
        display_cron_5field(&cron_7),
        cmd,
        next_str,
    );
    Ok(())
}

/// Type-guarded cron job mutation. Verifies the task is a command-type before mutating.
pub(crate) fn run_cron_mutate(id: &str, action: &str) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    match db.get_task_by_id(id)? {
        Some(task) if task.task_type == "command" => match action {
            "delete" => {
                db.delete_task(id)?;
                println!("Deleted cron job {}", short_id(id));
            }
            "pause" => {
                db.update_task_status(id, "paused")?;
                println!("Cron job {} paused", short_id(id));
            }
            "resume" => {
                db.update_task_status(id, "active")?;
                println!("Cron job {} resumed", short_id(id));
            }
            "run" => {
                let now = chrono::Utc::now().timestamp();
                db.update_task_next_run(id, Some(now))?;
                db.clear_task_retry(id)?;
                println!("Cron job {} queued for immediate execution", short_id(id));
            }
            _ => unreachable!(),
        },
        Some(_) => println!("Not a cron job: {id} (use `borg tasks` for prompt tasks)"),
        None => println!("Cron job not found: {id}"),
    }
    Ok(())
}
