use anyhow::{Context, Result};

pub(crate) async fn run_heal() -> Result<()> {
    let socket = crate::daemon_client::default_socket_path();
    if !socket.exists() {
        anyhow::bail!("Could not reach daemon. Start it with: borg service start");
    }
    let mut client = crate::daemon_client::connect(Some(&socket))
        .await
        .context("connect to borgd for heal")?;
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
    println!("Workflows pruned:      {}", r.workflows_pruned);
    println!("Messages purged:       {}", r.messages_purged);
    println!("Wedged runs recovered: {}", r.wedged_runs_recovered);
    println!("Backups pruned:        {}", r.backups_pruned);
    if r.log_bytes_truncated > 0 {
        println!("Log bytes truncated:   {}", r.log_bytes_truncated);
    }
    if !r.auto_backup_path.is_empty() {
        println!("Auto-backup written:   {}", r.auto_backup_path);
    }
    if !r.warnings.is_empty() {
        println!("Warnings:");
        for w in &r.warnings {
            println!("  - {w}");
        }
    }
}
