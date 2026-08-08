use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum StatusAction {
    /// Show evolution history timeline
    History,
    /// Show archetype score breakdown
    Archetypes,
    /// Show XP summary and recent feed
    Xp,
    /// Show compact evolution overview with readiness and momentum
    Evolution,
    /// Print an ASCII share card (--out writes to a file instead of stdout)
    Card {
        /// Output path. Omit to print to stdout.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
pub(crate) enum BondAction {
    /// Show recent bond event history
    History {
        /// Number of events to show
        #[arg(long, short, default_value_t = 20)]
        count: usize,
    },
}

/// Dispatch for `borg status ...`.
pub(crate) async fn dispatch_status(action: Option<StatusAction>) -> Result<()> {
    match action {
        None => run_status().await,
        Some(StatusAction::History) => run_status_history().await,
        Some(StatusAction::Archetypes) => run_status_archetypes().await,
        Some(StatusAction::Xp) => run_status_xp().await,
        Some(StatusAction::Evolution) => run_status_evolution().await,
        Some(StatusAction::Card { out }) => run_status_card(out).await,
    }
}

/// Dispatch for `borg bond ...`.
pub(crate) async fn dispatch_bond(action: Option<BondAction>) -> Result<()> {
    match action {
        Some(BondAction::History { count }) => run_bond_history(count).await,
        None => run_bond_status().await,
    }
}

pub(crate) async fn run_status() -> Result<()> {
    let now = chrono::Utc::now();
    let config = cortex::config::Config::load_from_db().await?;
    let db = cortex::db::Database::open_readonly().await?;

    // Emergency stop banner: the operator must see PAUSED before anything
    // else — every other section describes an agent that isn't taking work.
    if cortex::emergency_pause::is_engaged(&db).await {
        let (reason, since) = cortex::emergency_pause::details(&db).await;
        println!("PAUSED — new work halted (borg resume to continue)");
        if let Some(r) = &reason {
            println!("  reason: {r}");
        }
        if let Some(s) = since {
            let ago = now.timestamp().saturating_sub(s);
            let when = chrono::DateTime::from_timestamp(s, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| s.to_string());
            println!("  since:  {when} ({})", format_ago(ago));
        }
        println!();
    }

    if config.evolution.enabled {
        if let Ok(evo_state) = db.get_evolution_state().await {
            println!("Borg Status");
            println!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
            println!();
            print!("{}", cortex::evolution::format_status_section(&evo_state));
            println!();
        }
    }

    let state = db.get_vitals_state().await?;
    let state = cortex::vitals::apply_decay(&state, now);
    let mut drift = cortex::vitals::detect_drift(&state, now);
    let since = (now - chrono::Duration::days(7)).timestamp();
    let events = db.vitals_events_since(since).await?;
    if cortex::vitals::detect_failure_drift(&events) {
        drift.push(cortex::vitals::DriftFlag::RepeatedFailures);
    }
    println!("{}", cortex::vitals::format_status(&state, &events, &drift));

    if let Ok(bond_events) = db.get_all_bond_events().await {
        let bond_key = db.derive_hmac_key(cortex::bond::BOND_HMAC_DOMAIN);
        let bond_state = cortex::bond::replay_events_with_key(&bond_key, &bond_events);
        println!("Bond");
        println!(
            "  score        {}  {} ({})",
            format_bar(bond_state.score as usize, 10),
            bond_state.score,
            bond_state.level
        );
        println!();
    }

    Ok(())
}

/// Coarse "Xm ago" formatter for the pause banner.
pub(crate) fn format_ago(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h {}m ago", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h ago", secs / 86_400, (secs % 86_400) / 3600)
    }
}

pub(crate) fn format_bar(value: usize, width: usize) -> String {
    let filled = (value * width) / 100;
    format!(
        "{}{}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(width.saturating_sub(filled))
    )
}

pub(crate) async fn run_status_history() -> Result<()> {
    let config = cortex::config::Config::load_from_db().await?;
    if !config.evolution.enabled {
        println!(
            "Evolution system is disabled. Enable with: borg settings set evolution.enabled true"
        );
        return Ok(());
    }
    let db = cortex::db::Database::open_readonly().await?;
    let events = db.evolution_events_since(0).await?;
    let mut events = events;
    events.reverse();
    println!("{}", cortex::evolution::format_history(&events));
    Ok(())
}

pub(crate) async fn run_status_xp() -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let output = cortex::evolution::commands::dispatch(
        cortex::evolution::commands::EvolutionCommand::Xp,
        &db,
    )
    .await?;
    print!("{}", output.text);
    Ok(())
}

pub(crate) async fn run_status_evolution() -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let output = cortex::evolution::commands::dispatch(
        cortex::evolution::commands::EvolutionCommand::Evolution,
        &db,
    )
    .await?;
    print!("{}", output.text);
    Ok(())
}

pub(crate) async fn run_status_card(out: Option<std::path::PathBuf>) -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let output = cortex::evolution::commands::dispatch(
        cortex::evolution::commands::EvolutionCommand::Card { out },
        &db,
    )
    .await?;
    print!("{}", output.text);
    Ok(())
}

pub(crate) async fn run_status_archetypes() -> Result<()> {
    let config = cortex::config::Config::load_from_db().await?;
    if !config.evolution.enabled {
        println!(
            "Evolution system is disabled. Enable with: borg settings set evolution.enabled true"
        );
        return Ok(());
    }
    let db = cortex::db::Database::open_readonly().await?;
    let state = db.get_evolution_state().await?;
    println!("{}", cortex::evolution::format_archetype_scores(&state));
    Ok(())
}

pub(crate) async fn run_bond_status() -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let events = db.get_all_bond_events().await?;
    let bond_key = db.derive_hmac_key(cortex::bond::BOND_HMAC_DOMAIN);
    let state = cortex::bond::replay_events_with_key(&bond_key, &events);
    let correction_rate = cortex::bond::compute_correction_rate(&db).await;
    let routine_rate = cortex::bond::compute_routine_success_rate(&db).await;
    let pref_count = cortex::bond::compute_preference_learning_count(&db).await;
    let since = (chrono::Utc::now() - chrono::Duration::days(7)).timestamp();
    let recent = db.bond_events_since(since).await?;
    println!(
        "{}",
        cortex::bond::format_status(&state, correction_rate, routine_rate, pref_count, &recent)
    );
    Ok(())
}

pub(crate) async fn run_bond_history(count: usize) -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let events = db.bond_events_recent(count).await?;
    println!("{}", cortex::bond::format_history(&events));
    Ok(())
}

pub(crate) async fn run_usage() -> Result<()> {
    let db = cortex::db::Database::open_readonly().await?;
    let total_tokens = db.monthly_token_total().await?;
    let total_cost = db.monthly_total_cost().await?;
    let by_model = db.monthly_usage_by_model().await?;

    println!("Token usage for current month");
    println!("────────────────────────────────────────");
    println!("Total tokens: {total_tokens}");
    if let Some(cost) = total_cost {
        println!("Estimated cost: ${cost:.4}");
    }

    let cache_summary = {
        use chrono::Datelike;
        let now = chrono::Utc::now();
        let month_start = now
            .date_naive()
            .with_day(1)
            .unwrap_or_else(|| now.date_naive())
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp())
            .unwrap_or(0);
        db.cache_token_summary_since(month_start).await
    };
    if let Ok((prompt_sum, cached_sum, created_sum)) = cache_summary {
        if prompt_sum > 0 && (cached_sum > 0 || created_sum > 0) {
            let pct = cached_sum as f64 / prompt_sum as f64 * 100.0;
            println!(
                "Prompt cache: {cached_sum}/{prompt_sum} hit ({pct:.1}%), {created_sum} created"
            );
        }
    }

    if !by_model.is_empty() {
        println!();
        println!(
            "{:<40} {:>10} {:>10} {:>10} {:>10}",
            "Model", "Input", "Output", "Total", "Cost"
        );
        println!("{}", "─".repeat(84));
        for row in &by_model {
            let label = if row.model.is_empty() {
                "(unknown)".to_string()
            } else {
                row.model.clone()
            };
            let cost_str = match row.total_cost_usd {
                Some(c) => format!("${c:.4}"),
                None => "—".to_string(),
            };
            println!(
                "{:<40} {:>10} {:>10} {:>10} {:>10}",
                label, row.prompt_tokens, row.completion_tokens, row.total_tokens, cost_str
            );
        }
    }

    let config = cortex::config::Config::load_from_db()
        .await
        .unwrap_or_default();
    let budget_limit = config.budget.monthly_token_limit;
    if budget_limit > 0 {
        let pct = total_tokens as f64 / budget_limit as f64 * 100.0;
        println!();
        println!("Budget: {total_tokens}/{budget_limit} tokens ({pct:.1}%) used");
    }

    if let Ok(Some(snap)) = cortex::budgets::BudgetEngine::status_snapshot(&db, None).await {
        let period = match snap.reset_period {
            cortex::budgets::engine::ResetPeriod::Daily => "daily",
            cortex::budgets::engine::ResetPeriod::Weekly => "weekly",
            cortex::budgets::engine::ResetPeriod::Monthly => "monthly",
        };
        let elapsed_secs = (chrono::Utc::now().timestamp() - snap.current_period_start).max(1);
        let period_secs: i64 = match snap.reset_period {
            cortex::budgets::engine::ResetPeriod::Daily => 86_400,
            cortex::budgets::engine::ResetPeriod::Weekly => 7 * 86_400,
            cortex::budgets::engine::ResetPeriod::Monthly => 30 * 86_400,
        };
        let pace_str = pace_per_period(
            snap.spent.to_storage_string().as_str(),
            elapsed_secs,
            period_secs,
        );
        println!();
        println!(
            "Budgets · ${} / ${} {} · pace ~${}/{}",
            snap.spent.to_display_string(),
            snap.hard_cap.to_display_string(),
            period,
            pace_str,
            period
        );
    }

    Ok(())
}

/// Forward-looking pace: extrapolate the current period's spend out to a
/// full period at the current burn rate. The output is a UI hint only, so
/// f64 arithmetic is acceptable here — replay determinism only matters
/// for stored values.
fn pace_per_period(spent_storage: &str, elapsed_secs: i64, period_secs: i64) -> String {
    let spent = spent_storage.parse::<f64>().unwrap_or(0.0);
    if elapsed_secs <= 0 || period_secs <= 0 {
        return format!("{spent:.2}");
    }
    let pace = spent * (period_secs as f64) / (elapsed_secs as f64);
    format!("{pace:.2}")
}
