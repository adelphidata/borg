//! `borg audit verify` — replay the audit chain and report Ok / Tampered.

use anyhow::{Context, Result};

use crate::daemon_client::connect;

pub async fn run_verify() -> Result<()> {
    let mut client = connect(None).await.context("connect to daemon")?;
    let resp = client
        .verify_audit_chain()
        .await
        .context("verify audit chain")?;
    match resp.outcome.as_str() {
        "ok" => {
            println!("audit chain: OK");
            Ok(())
        }
        "tampered" => {
            eprintln!("audit chain: TAMPERED at seq={}", resp.tampered_seq);
            std::process::exit(1);
        }
        other => {
            eprintln!("audit chain: unexpected outcome {:?}", other);
            std::process::exit(2);
        }
    }
}
