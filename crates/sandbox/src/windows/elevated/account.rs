//! Sandbox local-user account identity.
//!
//! Two accounts are provisioned by `borg service install
//! --windows-sandbox`:
//!
//! | Account | Network | Purpose |
//! |---|---|---|
//! | `BorgSandboxOffline` | Blocked by WFP | network=false sandbox spawns |
//! | `BorgSandboxOnline` | Unrestricted (or proxy-allowlisted) | network=true sandbox spawns |
//!
//! Both accounts are local (no domain), in the `Users` local group,
//! and explicitly denied interactive logon (`SeDenyInteractiveLogonRight`)
//! and network logon (`SeDenyNetworkLogonRight`). Only batch logon
//! (`SeBatchLogonRight`) is granted, which is what `LogonUserW(LOGON32_LOGON_BATCH)`
//! requires.
//!
//! Account names are constants, not config — operator-visible
//! customization here would just be a way to bypass the WFP rules
//! that key off these specific names.

/// Sandbox account used for `network=false` spawns. WFP rules
/// installed by `borg service install --windows-sandbox` deny all
/// outbound network for this SID.
pub const OFFLINE_ACCOUNT: &str = "BorgSandboxOffline";

/// Sandbox account used for `network=true` spawns. By default
/// unrestricted; with `allowed_domains` set, traffic is routed
/// through a local egress proxy that enforces the allowlist (proxy
/// itself is future work — see ROADMAP).
pub const ONLINE_ACCOUNT: &str = "BorgSandboxOnline";

/// Sandbox account choice for a given policy. Trampolines that route
/// through Tier 2 use this to pick which account to `LogonUserW` as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxAccount {
    /// `BorgSandboxOffline` — WFP-blocked outbound.
    Offline,
    /// `BorgSandboxOnline` — unrestricted or proxy-gated outbound.
    Online,
}

impl SandboxAccount {
    /// Pick the appropriate account from a policy. `network=true` →
    /// `Online`; `network=false` → `Offline`. `allowed_domains` does
    /// not change the choice — the proxy gating is per-binding on the
    /// online account, not a separate identity.
    pub fn for_policy(policy: &crate::policy::SandboxPolicy) -> Self {
        if policy.network {
            Self::Online
        } else {
            Self::Offline
        }
    }

    /// Local-account name.
    pub fn account_name(self) -> &'static str {
        match self {
            Self::Offline => OFFLINE_ACCOUNT,
            Self::Online => ONLINE_ACCOUNT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SandboxPolicy;

    #[test]
    fn network_false_policy_picks_offline_account() {
        // Spec: any `network=false` policy must route through the
        // WFP-blocked account. Picking `Online` here would silently
        // grant outbound network to a policy that asked for none —
        // the exact "sandbox bypass" mode the dedicated accounts
        // exist to prevent.
        let policy = SandboxPolicy {
            network: false,
            ..Default::default()
        };
        assert_eq!(SandboxAccount::for_policy(&policy), SandboxAccount::Offline);
    }

    #[test]
    fn network_true_policy_picks_online_account() {
        // Spec: `network=true` routes through the unrestricted (or
        // proxy-gated) account. If this mapped to `Offline`, every
        // network-capable tool (web_fetch, browser) would silently
        // fail.
        let policy = SandboxPolicy {
            network: true,
            ..Default::default()
        };
        assert_eq!(SandboxAccount::for_policy(&policy), SandboxAccount::Online);
    }

    #[test]
    fn allowed_domains_does_not_change_account_choice() {
        // Spec: `allowed_domains` is a proxy-layer concern on the
        // online account; it does NOT shift a `network=true` policy
        // to a different account, and it does NOT lift a
        // `network=false` policy onto the online account "just to
        // reach the allowlisted domain." If a future change wants
        // domain-allowlisted offline mode, it needs explicit policy
        // surface, not implicit promotion.
        let mut policy = SandboxPolicy {
            network: true,
            ..Default::default()
        };
        policy.allowed_domains = vec!["api.github.com".into()];
        assert_eq!(SandboxAccount::for_policy(&policy), SandboxAccount::Online);

        policy.network = false;
        assert_eq!(SandboxAccount::for_policy(&policy), SandboxAccount::Offline);
    }

    #[test]
    fn account_names_are_stable_constants() {
        // Spec: WFP filter rules installed at `borg service install`
        // time key off the literal account name. A rename here
        // without a coordinated WFP-rules migration would silently
        // disable network blocking for the offline account.
        assert_eq!(SandboxAccount::Offline.account_name(), "BorgSandboxOffline");
        assert_eq!(SandboxAccount::Online.account_name(), "BorgSandboxOnline");
    }
}
