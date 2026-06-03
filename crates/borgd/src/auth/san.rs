//! Server-cert SAN derivation.
//!
//! The SAN follows from the actual bind address rather than a fixed
//! `["localhost"]`, so a remote-bound daemon mints a cert clients can verify:
//!
//! - No `--listen`, or loopback (`127.0.0.0/8` / `::1`): `["localhost"]`.
//! - Non-loopback bind: the operator MUST set `BORG_DAEMON_TLS_SAN` to the
//!   comma-separated DNS names (and/or IP literals) clients will connect to.
//!   Without it we fail loudly at startup rather than minting a cert with
//!   the wrong SAN — which would surface later as `BadCertificate` on the
//!   first remote handshake, with no hint about the cause.
//!
//! Keeps the "loopback-only today" property typed: the moment an operator
//! binds beyond loopback they're prompted for the hostname before the cert
//! is minted.

use anyhow::{anyhow, bail, Context, Result};
use std::net::SocketAddr;

/// Env var an operator sets when binding `--listen` to a non-loopback
/// address. Comma-separated DNS names; whitespace around each entry is
/// trimmed. Plain IP literals are allowed but should be the DNS form
/// clients actually use, since [`build_server_tls_config`] verifies SAN
/// match via rustls/webpki's case-insensitive DNS-name match.
///
/// [`build_server_tls_config`]: super::tls::build_server_tls_config
pub const SAN_ENV: &str = "BORG_DAEMON_TLS_SAN";

/// Default SAN entry for loopback binds (and for the UDS-only case where
/// no TCP listener exists). Mirrors the `domain_name("localhost")` set by
/// `daemon_client::connect_tcp`, so client and server agree out of the box.
pub const LOOPBACK_SAN: &str = "localhost";

/// Derive the server-cert SAN list from the `--listen` argument.
///
/// See [module docs][self] for the resolution rules. Returns a non-empty
/// vec; `localhost` is always sufficient for the UDS case and for clients
/// that dial the loopback TCP socket.
pub fn derive_server_san(listen_tcp: Option<&str>) -> Result<Vec<String>> {
    derive_server_san_with_mdns(listen_tcp, false, default_hostname_resolver)
}

/// Pure default hostname resolver — calls the OS. Pulled out so tests can
/// inject a deterministic value without setting the real machine hostname.
fn default_hostname_resolver() -> Option<String> {
    hostname::get()
        .ok()
        .and_then(|os| os.into_string().ok())
        .map(|h| h.trim_matches('.').to_string())
        .filter(|h| !h.is_empty())
}

/// SAN derivation with optional mDNS auto-expansion.
///
/// When `mdns_enabled = true` and the bind is non-loopback, the resolved
/// local hostname and its `.local` form are appended to whatever the
/// operator put in `BORG_DAEMON_TLS_SAN`. This keeps the "loopback-only
/// today" property unchanged when mDNS is off (default), but unlocks
/// "pair from this Mac to that Linux box" workflows when mDNS is on
/// without forcing the operator to hand-edit the SAN env var.
///
/// `hostname_resolver` is a function that returns the local hostname.
/// Tests inject a constant; production passes [`default_hostname_resolver`].
pub fn derive_server_san_with_mdns(
    listen_tcp: Option<&str>,
    mdns_enabled: bool,
    hostname_resolver: impl Fn() -> Option<String>,
) -> Result<Vec<String>> {
    let Some(addr) = listen_tcp else {
        return Ok(vec![LOOPBACK_SAN.into()]);
    };
    let parsed: SocketAddr = addr
        .parse()
        .with_context(|| format!("parsing --listen address `{addr}`"))?;

    if parsed.ip().is_loopback() {
        // mDNS on loopback is meaningless — peers can't reach 127.x — so
        // we deliberately don't auto-add the hostname here. Operators who
        // bind loopback get exactly what they got before this feature.
        return Ok(vec![LOOPBACK_SAN.into()]);
    }

    // Non-loopback bind: SAN must be operator-declared. A missing env var
    // is a configuration bug, not a runtime condition — fail at startup.
    let raw = std::env::var(SAN_ENV).map_err(|_| {
        anyhow!(
            "--listen {addr} binds a non-loopback address; set {SAN_ENV} to the \
             comma-separated DNS names (and/or IPs) clients will connect to, \
             otherwise mTLS handshakes will fail with BadCertificate"
        )
    })?;
    let mut names: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        bail!("{SAN_ENV} is set but contained no non-empty entries");
    }

    if mdns_enabled {
        if let Some(host) = hostname_resolver() {
            // DNS is case-insensitive; normalize to lowercase so the SAN
            // ends up in canonical form regardless of what `hostname` on
            // this platform happens to return (macOS preserves case;
            // some Linux distros uppercase).
            let host = host.trim_matches('.').to_ascii_lowercase();
            if !host.is_empty() {
                let local = format!("{host}.local");
                // Preserve operator-supplied order; append only entries
                // that aren't already present (case-insensitive).
                if !names.iter().any(|n| n.eq_ignore_ascii_case(&host)) {
                    names.push(host);
                }
                if !names.iter().any(|n| n.eq_ignore_ascii_case(&local)) {
                    names.push(local);
                }
            }
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `BORG_DAEMON_TLS_SAN` so the cargo-test
    /// thread pool doesn't race on process-wide env state.
    fn san_env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct SanEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
    }

    impl SanEnvGuard {
        fn set(value: Option<&str>) -> Self {
            let lock = san_env_lock();
            let prev = std::env::var_os(SAN_ENV);
            unsafe {
                match value {
                    Some(v) => std::env::set_var(SAN_ENV, v),
                    None => std::env::remove_var(SAN_ENV),
                }
            }
            Self { _lock: lock, prev }
        }
    }

    impl Drop for SanEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(SAN_ENV, v),
                    None => std::env::remove_var(SAN_ENV),
                }
            }
        }
    }

    #[test]
    fn no_listen_defaults_to_localhost() {
        let _g = SanEnvGuard::set(None);
        assert_eq!(derive_server_san(None).unwrap(), vec!["localhost"]);
    }

    #[test]
    fn loopback_ipv4_defaults_to_localhost() {
        // Even with the env var set, loopback binds use the well-known
        // SAN so client-side `domain_name("localhost")` keeps working.
        let _g = SanEnvGuard::set(Some("ignored.example.com"));
        assert_eq!(
            derive_server_san(Some("127.0.0.1:8009")).unwrap(),
            vec!["localhost"]
        );
    }

    #[test]
    fn loopback_ipv6_defaults_to_localhost() {
        let _g = SanEnvGuard::set(None);
        assert_eq!(
            derive_server_san(Some("[::1]:8009")).unwrap(),
            vec!["localhost"]
        );
    }

    #[test]
    fn non_loopback_without_env_returns_actionable_error() {
        // The error must name BOTH the bind addr (so operators know why
        // it triggered) and the env var (so they know how to fix it).
        // Vague messages here cost real on-call time — pin both signals.
        let _g = SanEnvGuard::set(None);
        let err = derive_server_san(Some("0.0.0.0:8009")).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("0.0.0.0:8009"),
            "error must mention the bind addr: {msg}"
        );
        assert!(
            msg.contains(SAN_ENV),
            "error must mention {SAN_ENV} so operators know the fix: {msg}"
        );
    }

    #[test]
    fn non_loopback_with_env_parses_comma_separated_names() {
        let _g = SanEnvGuard::set(Some("borg.example.com, fallback.internal ,, "));
        let san = derive_server_san(Some("10.0.0.5:8009")).unwrap();
        // Trims whitespace and drops empties from trailing commas. Order
        // is preserved so operators can put the primary name first.
        assert_eq!(san, vec!["borg.example.com", "fallback.internal"]);
    }

    #[test]
    fn malformed_listen_addr_is_rejected() {
        let _g = SanEnvGuard::set(None);
        let err = derive_server_san(Some("not-a-socket-addr")).unwrap_err();
        assert!(format!("{err}").contains("parsing --listen address"));
    }

    #[test]
    fn mdns_enabled_with_non_loopback_appends_hostname_and_dot_local() {
        // The whole point of `discovery.mdns_enabled`: pair from another
        // device on the LAN without the operator hand-editing the SAN env.
        // After flipping the flag, the local hostname + its `.local`
        // counterpart must both end up in the SAN, in addition to whatever
        // BORG_DAEMON_TLS_SAN already had.
        let _g = SanEnvGuard::set(Some("primary.example.com"));
        let san =
            derive_server_san_with_mdns(Some("10.0.0.5:8009"), true, || Some("mac-mini".into()))
                .unwrap();
        assert_eq!(
            san,
            vec!["primary.example.com", "mac-mini", "mac-mini.local"]
        );
    }

    #[test]
    fn mdns_disabled_excludes_hostname_even_with_resolver_returning_value() {
        // Regression target: silently auto-expanding the SAN regardless of
        // the discovery toggle would mint surprise certs that include a
        // hostname the operator didn't ask to publish. mDNS off must be
        // exactly today's behavior.
        let _g = SanEnvGuard::set(Some("primary.example.com"));
        let san =
            derive_server_san_with_mdns(Some("10.0.0.5:8009"), false, || Some("mac-mini".into()))
                .unwrap();
        assert_eq!(san, vec!["primary.example.com"]);
    }

    #[test]
    fn mdns_enabled_on_loopback_still_returns_just_localhost() {
        // mDNS auto-expansion is gated to non-loopback binds. Loopback
        // peers can never reach mac-mini.local, so adding it to the cert
        // would be noise at best and could surprise audits at worst.
        let _g = SanEnvGuard::set(None);
        let san =
            derive_server_san_with_mdns(Some("127.0.0.1:8009"), true, || Some("mac-mini".into()))
                .unwrap();
        assert_eq!(san, vec!["localhost"]);
    }

    #[test]
    fn mdns_enabled_deduplicates_hostname_when_already_in_env() {
        // An operator who already put their hostname in BORG_DAEMON_TLS_SAN
        // shouldn't end up with a duplicated SAN entry. webpki tolerates
        // duplicates but they're visible noise in `openssl x509 -text`.
        let _g = SanEnvGuard::set(Some("mac-mini, primary.example.com"));
        let san =
            derive_server_san_with_mdns(Some("10.0.0.5:8009"), true, || Some("MAC-MINI".into()))
                .unwrap();
        // Case-insensitive de-dup of `mac-mini`; `.local` still appended.
        assert_eq!(
            san,
            vec!["mac-mini", "primary.example.com", "mac-mini.local"]
        );
    }

    #[test]
    fn mdns_enabled_skips_appending_when_hostname_resolver_returns_none() {
        // Hostname lookup can legitimately fail (no /etc/hostname on some
        // minimal containers). The cert must still be issuable; we just
        // skip the auto-expansion silently.
        let _g = SanEnvGuard::set(Some("primary.example.com"));
        let san = derive_server_san_with_mdns(Some("10.0.0.5:8009"), true, || None).unwrap();
        assert_eq!(san, vec!["primary.example.com"]);
    }

    #[test]
    fn empty_san_env_is_rejected() {
        // Subtle: `BORG_DAEMON_TLS_SAN=""` should NOT silently fall back
        // to `localhost`. An operator who set the var to empty almost
        // certainly meant to set a value and made a shell-quoting mistake;
        // accepting it would mint a cert no remote client could verify.
        let _g = SanEnvGuard::set(Some(",  ,"));
        let err = derive_server_san(Some("10.0.0.5:8009")).unwrap_err();
        assert!(format!("{err}").contains("no non-empty entries"));
    }
}
