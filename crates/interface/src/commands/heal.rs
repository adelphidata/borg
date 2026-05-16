use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::commands::registry::{lookup, run_heal_action, CommandOutcome};

pub(crate) async fn run_heal() -> Result<()> {
    // Asserts the command is registered. If somebody removes `heal` from
    // the registry, this surface stops working at startup rather than
    // silently diverging from the TUI's `/heal`.
    if lookup("heal").is_none() {
        anyhow::bail!("internal: 'heal' command not registered in the command registry");
    }

    let socket = crate::daemon_client::default_socket_path();
    if !socket.exists() {
        anyhow::bail!("Could not reach daemon. Start it with: borg service start");
    }
    let client = crate::daemon_client::connect(Some(&socket))
        .await
        .context("connect to borgd for heal")?;
    let client = Arc::new(Mutex::new(client));
    let outcome = run_heal_action(client)
        .await
        .context("Admin.Heal via borgd")?;
    match outcome {
        CommandOutcome::Text(s) => println!("{s}"),
    }
    Ok(())
}
