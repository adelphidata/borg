//! GitHub Copilot OAuth credential discovery.
//!
//! Copilot tokens are resolved by shelling out to `gh auth token` and then
//! exchanging that token at GitHub's Copilot API endpoint for a short-lived
//! API token. Both halves of that flow are subprocess-driven and live in
//! the `borg auth pair copilot` command, where the operator's `gh` install
//! is the actual source of truth.
//!
//! For now, this discoverer is a no-op stub. `borg auth pair copilot` drives
//! the gh-CLI invocation directly and writes the resulting credential into
//! the pool via the existing `add_entry` path.

use std::path::Path;

use crate::provider::Provider;

use super::{CredentialSource, DiscoveredCredential};

pub struct CopilotSource;

impl CredentialSource for CopilotSource {
    fn name(&self) -> &'static str {
        "copilot"
    }

    fn provider(&self) -> Provider {
        Provider::CopilotOAuth
    }

    fn discover(&self, _home: &Path) -> anyhow::Result<Vec<DiscoveredCredential>> {
        // No on-disk scan — Copilot's resolved API token lives only in gh's
        // keychain entry and the short-lived exchange response. Pairing is
        // user-triggered via `borg auth pair copilot`.
        Ok(vec![])
    }
}
