pub(crate) mod audit;
pub(crate) mod backup;
pub(crate) mod client_token;
pub(crate) mod device_pair;
pub(crate) mod export;
pub(crate) mod mcp;
pub(crate) mod misc;
pub(crate) mod pairing;
pub(crate) mod projects;
pub(crate) mod providers;
pub(crate) mod revoke;
pub(crate) mod settings;
pub(crate) mod status;
pub(crate) mod tasks;
pub(crate) mod update_args;
pub(crate) mod webhook;

pub(crate) fn format_ts(ts: i64, fmt: &str) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format(fmt).to_string())
        .unwrap_or_else(|| "?".to_string())
}

pub(crate) fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max - 1;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
