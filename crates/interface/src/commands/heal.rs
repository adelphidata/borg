use anyhow::{Context, Result};

pub(crate) async fn run_heal() -> Result<()> {
    let mut client = crate::daemon_client::connect_default().await?;
    let report = client.heal().await.context("Admin.Heal via borgd")?;
    print_heal_report(&report);
    Ok(())
}

fn print_heal_report(r: &protocols::admin::HealReport) {
    if r.skipped {
        println!("Heal: skipped (another run is in progress).");
        return;
    }
    println!("Heal complete");
    println!("──────────────");
    println!("Logs pruned:           {}", r.pruned_logs);
    println!("Activity rows pruned:  {}", r.pruned_activity);
    println!("Embeddings evicted:    {}", r.evicted_embeddings);
    println!("Stalled tasks healed:  {}", r.healed_tasks);
    println!("Messages purged:       {}", r.messages_purged);
    println!("Wedged runs recovered: {}", r.wedged_runs_recovered);
    if r.log_bytes_truncated > 0 {
        println!("Log bytes truncated:   {}", r.log_bytes_truncated);
    }
    if !r.warnings.is_empty() {
        println!("Warnings:");
        for w in &r.warnings {
            println!("  - {w}");
        }
    }
}
