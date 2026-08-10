pub(crate) mod acp;
pub(crate) mod agents;
pub(crate) mod audit;
pub(crate) mod auth;
pub(crate) mod backup;
pub(crate) mod browser;
pub(crate) mod client_token;
pub(crate) mod device_pair;
pub(crate) mod doctor;
pub(crate) mod export;
pub(crate) mod heal;
pub(crate) mod mcp;
pub(crate) mod mcp_client;
pub(crate) mod memory;
pub(crate) mod misc;
pub(crate) mod pairing;
pub(crate) mod pause;
pub(crate) mod providers;
pub(crate) mod revoke;
pub(crate) mod sessions;
pub(crate) mod settings;
pub(crate) mod skills;
pub(crate) mod status;
pub(crate) mod tasks;
pub(crate) mod tunnel;
pub(crate) mod update_args;
pub(crate) mod wasm_plugin;
pub(crate) mod webhook;
pub(crate) mod worktree;

/// Canonical short timestamp format used across CLI/TUI surfaces.
pub(crate) const TS_FMT: &str = "%Y-%m-%d %H:%M";

pub(crate) fn format_ts(ts: i64, fmt: &str) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format(fmt).to_string())
        .unwrap_or_else(|| "?".to_string())
}

pub(crate) fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    cortex::util::truncate::truncate_to_width(s, max)
}

/// Resolve an id prefix against `ids`, or `None` when nothing matches.
///
/// An ambiguous prefix is an error rather than a pick: acting on the wrong
/// session or task is silent — it only surfaces once the right one is gone.
/// An exact hit wins outright, so a full id that also prefixes longer ids
/// still resolves.
pub(crate) fn resolve_unique_prefix(
    kind: &str,
    prefix: &str,
    ids: Vec<String>,
) -> anyhow::Result<Option<String>> {
    let mut matches: Vec<String> = ids
        .into_iter()
        .filter(|id| id.starts_with(prefix))
        .collect();
    if matches.iter().any(|id| id == prefix) {
        return Ok(Some(prefix.to_string()));
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        n => anyhow::bail!("'{prefix}' matches {n} {kind} — use a longer id prefix"),
    }
}

#[cfg(test)]
mod resolve_prefix_tests {
    use super::resolve_unique_prefix;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The whole point of this helper is that it never guesses. Pinning or
    /// clearing the wrong session is silent — it surfaces only once the right
    /// one has been purged — so an ambiguous prefix has to be refused, and an
    /// exact id must resolve even when it also prefixes longer ids.
    #[test]
    fn only_an_unambiguous_prefix_resolves() {
        let pool = ids(&["abc123", "abc789", "def456"]);

        assert_eq!(
            resolve_unique_prefix("sessions", "def", pool.clone()).unwrap(),
            Some("def456".to_string())
        );
        assert_eq!(
            resolve_unique_prefix("sessions", "zzz", pool.clone()).unwrap(),
            None
        );

        let err = resolve_unique_prefix("sessions", "abc", pool)
            .expect_err("an ambiguous prefix must not resolve to one of the candidates");
        let message = err.to_string();
        assert!(
            message.contains("matches 2 sessions"),
            "the error must say how ambiguous it was, got: {message}"
        );
    }

    /// A full id that happens to prefix a longer one must still resolve to
    /// itself rather than reading as ambiguous — otherwise the exact id the
    /// user copied out of a listing is unusable.
    #[test]
    fn an_exact_id_wins_over_the_longer_ids_it_prefixes() {
        let pool = ids(&["abc", "abcdef", "abcxyz"]);
        assert_eq!(
            resolve_unique_prefix("tasks", "abc", pool).unwrap(),
            Some("abc".to_string())
        );
    }
}
