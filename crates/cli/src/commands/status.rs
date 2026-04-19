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
pub(crate) fn dispatch_status(action: Option<StatusAction>) -> Result<()> {
    match action {
        None => run_status(),
        Some(StatusAction::History) => run_status_history(),
        Some(StatusAction::Archetypes) => run_status_archetypes(),
        Some(StatusAction::Xp) => run_status_xp(),
        Some(StatusAction::Evolution) => run_status_evolution(),
        Some(StatusAction::Card { out }) => run_status_card(out),
    }
}

/// Dispatch for `borg bond ...`.
pub(crate) fn dispatch_bond(action: Option<BondAction>) -> Result<()> {
    match action {
        Some(BondAction::History { count }) => run_bond_history(count),
        None => run_bond_status(),
    }
}

pub(crate) fn run_status() -> Result<()> {
    let now = chrono::Utc::now();
    let config = borg_core::config::Config::load_from_db()?;
    let db = borg_core::db::Database::open()?;

    if config.evolution.enabled {
        if let Ok(evo_state) = db.get_evolution_state() {
            println!("Borg Status");
            println!("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}");
            println!();
            print!(
                "{}",
                borg_core::evolution::format_status_section(&evo_state)
            );
            println!();
        }
    }

    let state = db.get_vitals_state()?;
    let state = borg_core::vitals::apply_decay(&state, now);
    let mut drift = borg_core::vitals::detect_drift(&state, now);
    let since = (now - chrono::Duration::days(7)).timestamp();
    let events = db.vitals_events_since(since)?;
    if borg_core::vitals::detect_failure_drift(&events) {
        drift.push(borg_core::vitals::DriftFlag::RepeatedFailures);
    }
    println!(
        "{}",
        borg_core::vitals::format_status(&state, &events, &drift)
    );

    if let Ok(bond_events) = db.get_all_bond_events() {
        let bond_key = db.derive_hmac_key(borg_core::bond::BOND_HMAC_DOMAIN);
        let bond_state = borg_core::bond::replay_events_with_key(&bond_key, &bond_events);
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

pub(crate) fn format_bar(value: usize, width: usize) -> String {
    let filled = (value * width) / 100;
    format!(
        "{}{}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(width.saturating_sub(filled))
    )
}

pub(crate) fn run_status_history() -> Result<()> {
    let config = borg_core::config::Config::load_from_db()?;
    if !config.evolution.enabled {
        println!(
            "Evolution system is disabled. Enable with: borg settings set evolution.enabled true"
        );
        return Ok(());
    }
    let db = borg_core::db::Database::open()?;
    let events = db.evolution_events_since(0)?;
    let mut events = events;
    events.reverse();
    println!("{}", borg_core::evolution::format_history(&events));
    Ok(())
}

pub(crate) fn run_status_xp() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let output = borg_core::evolution::commands::dispatch(
        borg_core::evolution::commands::EvolutionCommand::Xp,
        &db,
    )?;
    print!("{}", output.text);
    Ok(())
}

pub(crate) fn run_status_evolution() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let output = borg_core::evolution::commands::dispatch(
        borg_core::evolution::commands::EvolutionCommand::Evolution,
        &db,
    )?;
    print!("{}", output.text);
    Ok(())
}

pub(crate) fn run_status_card(out: Option<std::path::PathBuf>) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let output = borg_core::evolution::commands::dispatch(
        borg_core::evolution::commands::EvolutionCommand::Card { out },
        &db,
    )?;
    print!("{}", output.text);
    Ok(())
}

pub(crate) fn run_status_archetypes() -> Result<()> {
    let config = borg_core::config::Config::load_from_db()?;
    if !config.evolution.enabled {
        println!(
            "Evolution system is disabled. Enable with: borg settings set evolution.enabled true"
        );
        return Ok(());
    }
    let db = borg_core::db::Database::open()?;
    let state = db.get_evolution_state()?;
    println!("{}", borg_core::evolution::format_archetype_scores(&state));
    Ok(())
}

pub(crate) fn run_bond_status() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let events = db.get_all_bond_events()?;
    let bond_key = db.derive_hmac_key(borg_core::bond::BOND_HMAC_DOMAIN);
    let state = borg_core::bond::replay_events_with_key(&bond_key, &events);
    let correction_rate = borg_core::bond::compute_correction_rate(&db);
    let routine_rate = borg_core::bond::compute_routine_success_rate(&db);
    let pref_count = borg_core::bond::compute_preference_learning_count(&db);
    let since = (chrono::Utc::now() - chrono::Duration::days(7)).timestamp();
    let recent = db.bond_events_since(since)?;
    println!(
        "{}",
        borg_core::bond::format_status(&state, correction_rate, routine_rate, pref_count, &recent)
    );
    Ok(())
}

pub(crate) fn run_bond_history(count: usize) -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let events = db.bond_events_recent(count)?;
    println!("{}", borg_core::bond::format_history(&events));
    Ok(())
}

pub(crate) fn run_usage() -> Result<()> {
    let db = borg_core::db::Database::open()?;
    let total_tokens = db.monthly_token_total()?;
    let total_cost = db.monthly_total_cost()?;
    let by_model = db.monthly_usage_by_model()?;

    println!("Token usage for current month");
    println!("────────────────────────────────────────");
    println!("Total tokens: {total_tokens}");
    if let Some(cost) = total_cost {
        println!("Estimated cost: ${cost:.4}");
    }

    if let Ok((prompt_sum, cached_sum, created_sum)) = {
        use chrono::Datelike;
        let now = chrono::Utc::now();
        let month_start = now
            .date_naive()
            .with_day(1)
            .unwrap_or_else(|| now.date_naive())
            .and_hms_opt(0, 0, 0)
            .map(|dt| dt.and_utc().timestamp())
            .unwrap_or(0);
        db.cache_token_summary_since(month_start)
    } {
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

    let config = borg_core::config::Config::load_from_db().unwrap_or_default();
    let budget_limit = config.budget.monthly_token_limit;
    if budget_limit > 0 {
        let pct = total_tokens as f64 / budget_limit as f64 * 100.0;
        println!();
        println!("Budget: {total_tokens}/{budget_limit} tokens ({pct:.1}%) used");
    }

    Ok(())
}
