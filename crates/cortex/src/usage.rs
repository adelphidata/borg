//! `/usage` text formatter — token spend, model usage, tool-call
//! distribution, and channel activity over a rolling time window.
//!
//! The daemon owns the formatter (mirroring `GetXpText` / `GetEvolutionText`
//! in `protocols/proto/status.proto`); the TUI popup renders the returned
//! string verbatim. A single function so the omission rules — hide the cost
//! section when no rate-card data is present, omit Tools / Channels sections
//! that have zero rows — are testable in one place.

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};

use crate::db::{Database, ModelUsageRow};

/// Default time window when the caller passes 0 or no arg.
pub const DEFAULT_DAYS: u32 = 30;
/// Hard upper bound — token_usage is unbounded retention but rendering
/// 5 years of activity in a popup is a footgun.
pub const MAX_DAYS: u32 = 365;
/// Minimum (anything smaller is almost certainly a bug — `/usage 0`
/// should land on `DEFAULT_DAYS`, not silently empty).
pub const MIN_DAYS: u32 = 1;
/// Cap on rows rendered in the Models section.
pub const TOP_MODELS: usize = 10;
/// Cap on rows rendered in the Tools section.
pub const TOP_TOOLS: usize = 10;
/// Cap on rows rendered in the Channels section.
pub const TOP_CHANNELS: usize = 10;

/// Resolve the requested window to a clamped day count. `days = 0` means
/// "use default" (the proto contract reuses 0 as sentinel since `uint32`
/// has no `Option`).
pub fn clamp_days(days: u32) -> u32 {
    if days == 0 {
        return DEFAULT_DAYS;
    }
    days.clamp(MIN_DAYS, MAX_DAYS)
}

/// Render `/usage` as a multi-section plain-text block over a window of
/// `days` days. `days = 0` resolves to [`DEFAULT_DAYS`].
///
/// Sections, in order — each separated by a blank line:
///
/// 1. Header (`Usage ({days} days, since YYYY-MM-DD)`)
/// 2. Activity (sessions / messages / tool calls)
/// 3. Tokens (prompt / completion / cached / cache-write / total)
/// 4. Cost — **omitted entirely** when no row in window has a non-null `cost_usd`
/// 5. Models (top [`TOP_MODELS`])
/// 6. Tools — **omitted** when zero tool calls in window
/// 7. Channels — **omitted** when zero channel activity in window
pub async fn format_usage_text(db: &Database, days: u32) -> Result<String> {
    let resolved = clamp_days(days);
    let now = Utc::now().timestamp();
    let since_ts = now - (resolved as i64) * 86_400;

    let summary = db.token_summary_since(since_ts).await?;
    let model_usage = db.usage_by_model_since(since_ts).await?;
    let tool_calls = db.tool_call_counts_since(since_ts, TOP_TOOLS).await?;
    let channels = db.channel_activity_since(since_ts).await?;
    let session_count = db.session_count_since(since_ts).await?;
    let message_count = db.message_count_since(since_ts).await?;
    let total_tool_calls: u64 = tool_calls.iter().map(|(_, n)| *n).sum();

    let since_str = format_since_date(since_ts);
    let mut out = String::new();
    out.push_str(&format!("Usage ({resolved} days, since {since_str})\n"));

    out.push('\n');
    out.push_str("Activity:\n");
    out.push_str(&format!(
        "  {} sessions · {} messages · {} tool calls\n",
        fmt_count(session_count),
        fmt_count(message_count),
        fmt_count(total_tool_calls),
    ));

    out.push('\n');
    out.push_str("Tokens:\n");
    out.push_str(&format!(
        "  prompt {} · completion {} · cached {} · cache-write {} · total {}\n",
        fmt_count(summary.prompt),
        fmt_count(summary.completion),
        fmt_count(summary.cached_input),
        fmt_count(summary.cache_creation),
        fmt_count(summary.total),
    ));

    if summary.cost_present {
        out.push('\n');
        out.push_str("Cost:\n");
        out.push_str(&format!(
            "  ${:.4} estimated\n",
            summary.cost.unwrap_or(0.0)
        ));
    }

    if !model_usage.is_empty() {
        out.push('\n');
        out.push_str("Models:\n");
        for row in model_usage.iter().take(TOP_MODELS) {
            out.push_str(&format_model_row(row));
        }
    }

    if !tool_calls.is_empty() {
        out.push('\n');
        out.push_str("Tools:\n");
        for (name, count) in tool_calls.iter().take(TOP_TOOLS) {
            let pct = if total_tool_calls > 0 {
                (*count as f64) * 100.0 / (total_tool_calls as f64)
            } else {
                0.0
            };
            out.push_str(&format!(
                "  {} — {} calls ({:.1}%)\n",
                name,
                fmt_count(*count),
                pct
            ));
        }
    }

    if !channels.is_empty() {
        out.push('\n');
        out.push_str("Channels:\n");
        for ch in channels.iter().take(TOP_CHANNELS) {
            out.push_str(&format!(
                "  {} — {} msgs (last active: {})\n",
                ch.channel_name,
                fmt_count(ch.message_count),
                format_relative(now, ch.last_active_unix),
            ));
        }
    }

    Ok(out)
}

fn format_model_row(row: &ModelUsageRow) -> String {
    let label = if row.provider.is_empty() {
        row.model.clone()
    } else if row.model.is_empty() {
        row.provider.clone()
    } else {
        format!("{}/{}", row.provider, row.model)
    };
    match row.total_cost_usd {
        Some(cost) => format!(
            "  {} — {} tokens (prompt {}, completion {}) · ${:.4}\n",
            label,
            fmt_count(row.total_tokens),
            fmt_count(row.prompt_tokens),
            fmt_count(row.completion_tokens),
            cost,
        ),
        None => format!(
            "  {} — {} tokens (prompt {}, completion {})\n",
            label,
            fmt_count(row.total_tokens),
            fmt_count(row.prompt_tokens),
            fmt_count(row.completion_tokens),
        ),
    }
}

fn format_since_date(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt: DateTime<Utc>| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "—".to_string())
}

fn format_relative(now: i64, ts: i64) -> String {
    let delta = (now - ts).max(0);
    if delta < 60 {
        return "just now".to_string();
    }
    if delta < 3_600 {
        return format!("{}m ago", delta / 60);
    }
    if delta < 86_400 {
        return format!("{}h ago", delta / 3_600);
    }
    format!("{}d ago", delta / 86_400)
}

fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_days_zero_resolves_to_default_not_min() {
        // Spec: the proto wire uses 0 as the "no arg" sentinel because
        // uint32 has no Option. A regression that returned MIN_DAYS (1)
        // for 0 would silently shrink every default request to 1 day.
        assert_eq!(clamp_days(0), DEFAULT_DAYS);
        assert_ne!(clamp_days(0), MIN_DAYS, "0 must NOT collapse to MIN_DAYS");
    }

    #[test]
    fn clamp_days_clamps_to_min_and_max() {
        // Boundary: 1 stays at 1, max stays at max, beyond max collapses
        // to max, between is preserved. A wrong direction on the clamp
        // would silently render years of usage in a popup.
        assert_eq!(clamp_days(1), 1);
        assert_eq!(clamp_days(MAX_DAYS), MAX_DAYS);
        assert_eq!(clamp_days(MAX_DAYS + 100), MAX_DAYS);
        assert_eq!(clamp_days(30), 30);
        assert_eq!(clamp_days(u32::MAX), MAX_DAYS);
    }

    #[test]
    fn fmt_count_inserts_commas_at_thousand_boundaries() {
        // Spec encoded as a table — a regression that mis-counted from
        // the right (e.g. counting from the left) would put commas in
        // the wrong place: "1234567" → "12,34,567" instead of "1,234,567".
        let cases: &[(u64, &str)] = &[
            (0, "0"),
            (1, "1"),
            (12, "12"),
            (123, "123"),
            (1_000, "1,000"),
            (1_234, "1,234"),
            (12_345, "12,345"),
            (123_456, "123,456"),
            (1_234_567, "1,234,567"),
            (1_000_000, "1,000,000"),
            (1_234_567_890, "1,234,567,890"),
            (u64::MAX, "18,446,744,073,709,551,615"),
        ];
        for (input, expected) in cases {
            assert_eq!(fmt_count(*input), *expected, "fmt_count({input})");
        }
    }

    #[test]
    fn format_relative_buckets_by_unit() {
        // Boundaries: a regression that swapped `<` for `<=` would
        // shift the seconds→minutes transition by one second and surface
        // as "59m ago" vs "just now" inconsistencies.
        let now = 1_700_000_000_i64;
        assert_eq!(format_relative(now, now), "just now");
        assert_eq!(format_relative(now, now - 59), "just now");
        assert_eq!(format_relative(now, now - 60), "1m ago");
        assert_eq!(format_relative(now, now - 3_599), "59m ago");
        assert_eq!(format_relative(now, now - 3_600), "1h ago");
        assert_eq!(format_relative(now, now - 86_399), "23h ago");
        assert_eq!(format_relative(now, now - 86_400), "1d ago");
        assert_eq!(format_relative(now, now - 7 * 86_400), "7d ago");
    }

    #[test]
    fn format_relative_future_timestamps_clamp_to_just_now() {
        // Spec: future ts (clock skew between daemon and DB row) must
        // not produce a negative-number string — `delta = (now - ts).max(0)`
        // is the load-bearing line; a refactor that dropped `.max(0)`
        // would surface as "-1234m ago" in the UI.
        let now = 1_700_000_000_i64;
        assert_eq!(format_relative(now, now + 5_000), "just now");
    }

    #[test]
    fn format_since_date_is_iso_yyyy_mm_dd() {
        // The date string is consumed by users; lock the format.
        // 1_700_000_000 = 2023-11-14T22:13:20Z.
        let s = format_since_date(1_700_000_000);
        assert_eq!(s, "2023-11-14");
    }

    #[test]
    fn format_since_date_handles_unsupported_timestamps_via_sentinel() {
        // Spec: out-of-range or ambiguous timestamps fall back to "—"
        // instead of panicking. A `.unwrap()` regression in the chrono
        // adapter would crash the formatter for any large value.
        let s = format_since_date(i64::MAX);
        assert_eq!(s, "—");
        let s = format_since_date(i64::MIN);
        assert_eq!(s, "—");
    }

    #[test]
    fn format_model_row_handles_missing_provider_or_model() {
        // Spec: empty-string provider falls back to model alone; empty
        // model falls back to provider alone; both present joins with
        // a slash. A regression to "/{model}" or "{provider}/" formats
        // would surface dangling separators in the UI.
        let row = ModelUsageRow {
            provider: "".into(),
            model: "claude".into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            total_cost_usd: None,
        };
        let s = format_model_row(&row);
        assert!(s.contains("claude"), "label must include model name");
        assert!(!s.contains("/claude"), "must NOT have dangling slash");

        let row = ModelUsageRow {
            provider: "openai".into(),
            model: "".into(),
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            total_cost_usd: None,
        };
        let s = format_model_row(&row);
        assert!(s.contains("openai"));
        assert!(!s.contains("openai/"));

        let row = ModelUsageRow {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            prompt_tokens: 1234,
            completion_tokens: 5678,
            total_tokens: 6912,
            total_cost_usd: Some(0.5678),
        };
        let s = format_model_row(&row);
        assert!(s.contains("openai/gpt-4o"));
        // Cost present → must surface $X.XXXX, four-decimal precision.
        assert!(s.contains("$0.5678"), "must format cost: {s}");
        // Token counts must be comma-formatted via fmt_count.
        assert!(s.contains("1,234"), "prompt tokens commafied: {s}");
        assert!(s.contains("5,678"), "completion tokens commafied: {s}");
        assert!(s.contains("6,912"), "total tokens commafied: {s}");
    }

    #[test]
    fn format_model_row_without_cost_omits_dollar_segment() {
        // Spec: rows with None cost must NOT render "$0.00" — the cost
        // section is a separate concept and showing $0 here would imply
        // we have rate-card data when we don't.
        let row = ModelUsageRow {
            provider: "ollama".into(),
            model: "llama3".into(),
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            total_cost_usd: None,
        };
        let s = format_model_row(&row);
        assert!(!s.contains('$'), "must NOT show $ when cost is None: {s}");
    }

    #[tokio::test]
    async fn format_usage_text_omits_cost_section_when_no_rate_card_data() {
        // End-to-end: every row has None cost → no "Cost:" header in the
        // formatted output. The Activity and Tokens sections still appear.
        let db = crate::db::Database::test_db().await;
        db.log_token_usage(100, 50, 150, "ollama", "llama3", None)
            .await
            .unwrap();
        let out = format_usage_text(&db, 30).await.expect("format");
        assert!(
            !out.contains("Cost:"),
            "no rate data must omit Cost section: {out}"
        );
        assert!(out.contains("Tokens:"));
        assert!(out.contains("Activity:"));
    }

    #[tokio::test]
    async fn format_usage_text_includes_cost_when_any_row_has_cost() {
        // Inverse: a single priced row must light up the Cost section
        // with the four-decimal estimate.
        let db = crate::db::Database::test_db().await;
        db.log_token_usage(100, 50, 150, "openai", "gpt-4o", Some(0.0123))
            .await
            .unwrap();
        let out = format_usage_text(&db, 30).await.expect("format");
        assert!(out.contains("Cost:"));
        assert!(out.contains("$0.0123"));
    }

    #[tokio::test]
    async fn format_usage_text_header_includes_day_count_and_since_date() {
        // The header is the user's primary orientation: "Usage (30 days,
        // since YYYY-MM-DD)". A regression that broke the substitution
        // would surface as `{days}` literally in the popup.
        let db = crate::db::Database::test_db().await;
        let out = format_usage_text(&db, 7).await.expect("format");
        assert!(out.starts_with("Usage (7 days, since "), "got: {out}");
        assert!(!out.contains("{days}"));
    }

    #[tokio::test]
    async fn format_usage_text_zero_days_resolves_to_default() {
        // Spec: the proto sentinel must round-trip through the formatter
        // to the default window, not collapse to "0 days".
        let db = crate::db::Database::test_db().await;
        let out = format_usage_text(&db, 0).await.expect("format");
        assert!(
            out.contains(&format!("Usage ({DEFAULT_DAYS} days,")),
            "zero must resolve to DEFAULT_DAYS={DEFAULT_DAYS}: {out}"
        );
    }
}

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;
    use rusqlite::params;

    fn now() -> i64 {
        Utc::now().timestamp()
    }

    /// Insert a `token_usage` row with explicit fields. The timestamp is
    /// what makes the row fall in / out of `since_ts`.
    #[allow(clippy::too_many_arguments)]
    fn insert_token_row(
        db: &Database,
        ts: i64,
        prompt: i64,
        completion: i64,
        total: i64,
        cached_in: i64,
        cache_creation: i64,
        provider: &str,
        model: &str,
        cost: Option<f64>,
    ) {
        db.conn().execute(
            "INSERT INTO token_usage (timestamp, prompt_tokens, completion_tokens, total_tokens,
                                       provider, model, cost_usd, cached_input_tokens, cache_creation_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![ts, prompt, completion, total, provider, model, cost, cached_in, cache_creation],
        ).expect("insert token_usage");
    }

    fn insert_message(
        db: &Database,
        session_id: &str,
        created_at: i64,
        tool_calls_json: Option<&str>,
    ) {
        db.conn()
            .execute(
                "INSERT INTO messages (session_id, role, content, tool_calls_json, created_at)
                 VALUES (?1, 'assistant', NULL, ?2, ?3)",
                params![session_id, tool_calls_json, created_at],
            )
            .expect("insert message");
    }

    fn ensure_session(db: &Database, id: &str, ts: i64) {
        db.upsert_session(id, ts, ts, 0, "openai/gpt-4o", "test")
            .expect("upsert session");
    }

    #[test]
    fn formatter_omits_cost_section_when_no_rate_card_data() {
        let db = Database::test_db();
        ensure_session(&db, "s1", now());
        // No cost_usd on any row → cost_present = false.
        insert_token_row(&db, now(), 100, 50, 150, 0, 0, "ollama", "llama3", None);
        let out = format_usage_text(&db, 30).unwrap();
        // The literal section header "Cost:" must not appear when no row has
        // a non-null cost_usd. Failure mode being guarded: a regression that
        // renders "$0.00" when no rate card is loaded ships a misleading
        // "pricing unreliable" number to the user.
        assert!(
            !out.contains("Cost:"),
            "Cost section should be hidden when cost_present is false. Got:\n{out}"
        );
    }

    #[test]
    fn formatter_includes_cost_section_when_rate_card_present() {
        let db = Database::test_db();
        ensure_session(&db, "s1", now());
        insert_token_row(
            &db,
            now(),
            500,
            200,
            700,
            0,
            0,
            "openai",
            "gpt-4o",
            Some(0.0125),
        );
        let out = format_usage_text(&db, 30).unwrap();
        assert!(
            out.contains("Cost:"),
            "Cost section should render. Got:\n{out}"
        );
        assert!(
            out.contains("0.0125"),
            "Cost amount should appear formatted. Got:\n{out}"
        );
    }

    #[test]
    fn formatter_omits_tools_section_when_no_tool_calls() {
        let db = Database::test_db();
        ensure_session(&db, "s1", now());
        // Insert a plain message without tool_calls_json.
        insert_message(&db, "s1", now(), None);
        let out = format_usage_text(&db, 30).unwrap();
        assert!(
            !out.contains("Tools:"),
            "Tools section should be hidden when zero tool calls. Got:\n{out}"
        );
    }

    #[test]
    fn formatter_omits_channels_section_when_no_channel_messages() {
        let db = Database::test_db();
        ensure_session(&db, "s1", now());
        let out = format_usage_text(&db, 30).unwrap();
        assert!(
            !out.contains("Channels:"),
            "Channels section should be hidden when channel_messages is empty. Got:\n{out}"
        );
    }

    #[test]
    fn formatter_caps_top_models_at_ten() {
        let db = Database::test_db();
        ensure_session(&db, "s1", now());
        // 15 distinct (provider, model) pairs → only 10 should render.
        for i in 0..15 {
            insert_token_row(
                &db,
                now(),
                100 + i,
                50 + i,
                150 + i * 2,
                0,
                0,
                "openai",
                &format!("model-{i:02}"),
                None,
            );
        }
        let out = format_usage_text(&db, 30).unwrap();
        // Count `openai/model-` occurrences in the rendered text.
        let rendered = out.matches("openai/model-").count();
        assert_eq!(
            rendered, TOP_MODELS,
            "Models section should cap at TOP_MODELS rows; rendered {rendered}.\nOutput:\n{out}"
        );
    }

    #[test]
    fn clamp_days_handles_zero_and_overflow() {
        // Zero is the proto's "use default" sentinel.
        assert_eq!(clamp_days(0), DEFAULT_DAYS);
        // Above max clamps to MAX_DAYS, not 0 or default.
        assert_eq!(clamp_days(100_000), MAX_DAYS);
        // Below max passes through.
        assert_eq!(clamp_days(7), 7);
    }

    #[test]
    fn formatter_header_reflects_clamped_window() {
        // The proto contract says `days = 0` means "use default" — a regression
        // where the formatter took the literal 0 would render "0 days, since
        // today" and quietly produce an empty popup. Assert the header text
        // visibly reflects DEFAULT_DAYS so end-to-end users can see what
        // window they're looking at.
        let db = Database::test_db();
        let out = format_usage_text(&db, 0).expect("zero clamps to default");
        assert!(
            out.contains(&format!("{DEFAULT_DAYS} days")),
            "header must reflect the default window. Got:\n{out}"
        );

        // Above MAX_DAYS clamps to MAX_DAYS, not to default.
        let out_max = format_usage_text(&db, 100_000).expect("max-clamp");
        assert!(
            out_max.contains(&format!("{MAX_DAYS} days")),
            "header must reflect MAX_DAYS when over the cap. Got:\n{out_max}"
        );
    }
}
