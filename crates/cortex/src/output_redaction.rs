//! Output-side redaction policy.
//!
//! Four sinks expose agent-generated content to a human or to a persisted
//! DB surface an operator inspects:
//!
//! 1. Gateway → channel delivery (Telegram, Slack, Discord, …).
//! 2. Gateway → `channel_messages` SQLite log of outbound text.
//! 3. TUI transcript scrollback.
//! 4. Log files at `~/.borg/logs/`.
//!
//! Input-side defense already strips secrets from prompts (see
//! [`crate::secrets::redact_secrets`] in the prompt-injection layer). Output
//! redaction is the symmetric guard: if an agent legitimately reads
//! `~/.aws/credentials` and echoes the result, none of the sinks above
//! should ever forward the raw key. Critical for shared-screen and
//! posted-log scenarios.
//!
//! The single funnel is [`RedactionPolicy::apply`]. Every sink call site
//! goes through it — see the chokepoint guard test
//! `output_redaction_chokepoint_guard.rs`.

use std::borrow::Cow;
use std::sync::{Arc, Mutex, OnceLock};

use crate::db::Database;
use crate::secrets::{redact_with_audit, RedactionHit};

/// Where the redacted text is about to be delivered. Round-tripped into the
/// audit log via [`OutputSink::as_audit_label`] so an operator inspecting
/// `redaction_audit` can correlate hits with the surface they leaked toward.
#[derive(Debug, Clone)]
pub enum OutputSink<'a> {
    /// Gateway channel delivery. The string is the channel name —
    /// `"telegram"`, `"slack"`, `"discord"`, `"teams"`, `"googlechat"`,
    /// `"signal"`, `"twilio"`, `"imessage"`, or a script-channel name.
    /// Borrowed so callers can pass `&channel_name` without allocating.
    Channel(&'a str),
    /// TUI transcript insertion (assistant / tool output cell).
    Tui,
    /// Log file write.
    Log,
}

impl<'a> OutputSink<'a> {
    /// Canonical label persisted to `redaction_audit.sink`. Stable across
    /// releases — changing it would silently reshape downstream queries.
    pub fn as_audit_label(&self) -> String {
        match self {
            OutputSink::Channel(name) => format!("channel:{name}"),
            OutputSink::Tui => "tui".to_string(),
            OutputSink::Log => "log".to_string(),
        }
    }
}

/// Audit-sink callback. Called once per `apply()` that produced at least one
/// hit, with the full hit list batched (not once per hit). Implementations
/// **must not panic** — `apply()` does not catch unwinding, so a panicking
/// closure would abort the calling task. Internally fallible work (DB writes,
/// channel sends) must be logged via `tracing::warn!` and swallowed so a
/// transient hiccup never blocks delivery of an already-redacted message.
pub type RedactionAuditFn = dyn Fn(&str, Option<&str>, &[RedactionHit]) + Send + Sync;

/// Output-redaction policy. Constructed once at daemon startup from
/// `Config::security.output_redaction` plus the shared DB handle, then
/// cloned cheaply into each sink (gateway dispatcher, TUI `App` state,
/// logging path via the global install).
#[derive(Clone)]
pub struct RedactionPolicy {
    enabled: bool,
    audit: Option<Arc<RedactionAuditFn>>,
}

impl RedactionPolicy {
    /// Production constructor: redaction governed by `enabled`, hits
    /// persisted to the shared `redaction_audit` table.
    pub fn from_db(enabled: bool, db: Arc<Mutex<Database>>) -> Self {
        let audit: Arc<RedactionAuditFn> = Arc::new(move |sink, ctx, hits| {
            let guard = match db.lock() {
                Ok(g) => g,
                Err(poisoned) => {
                    tracing::warn!(
                        sink = %sink,
                        "redaction audit DB mutex poisoned; recovering and recording anyway"
                    );
                    poisoned.into_inner()
                }
            };
            if let Err(e) = guard.record_redactions(sink, ctx, hits) {
                tracing::warn!(
                    sink = %sink,
                    error = %e,
                    "failed to persist redaction_audit row; redaction still applied"
                );
            }
        });
        Self {
            enabled,
            audit: Some(audit),
        }
    }

    /// Construct with a caller-supplied audit closure. Used by tests that
    /// want to observe hits without standing up a SQLite database.
    pub fn with_audit(enabled: bool, audit: Arc<RedactionAuditFn>) -> Self {
        Self {
            enabled,
            audit: Some(audit),
        }
    }

    /// Redaction enabled, no audit sink. Used by stdout-only builds and
    /// some unit tests; redaction still happens, hits are dropped.
    pub fn enabled_no_audit() -> Self {
        Self {
            enabled: true,
            audit: None,
        }
    }

    /// Disabled policy. [`apply`] always returns `Cow::Borrowed` unchanged
    /// and never writes audit rows — the operator-toggle contract.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            audit: None,
        }
    }

    /// True if the operator toggle is on. Exposed for diagnostics
    /// (`borg doctor`, status JSON); the runtime path uses [`apply`].
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Redact `text` for the given sink. When the policy is disabled the
    /// input is returned `Cow::Borrowed` with zero allocation and no audit
    /// write — that's the operator-toggle contract.
    ///
    /// When enabled and the detector finds at least one match, the audit
    /// closure is invoked once with the full hit list (label + count, never
    /// the secret value).
    ///
    /// `context_key` carries caller-supplied correlation data — typically
    /// `"{sender_id}:{thread_id}"` for channel sinks, the session id for the
    /// TUI, or `None` for logs. It is **not** derived from `text`, so the
    /// secret value never enters the audit log.
    pub fn apply<'a>(
        &self,
        sink: OutputSink<'_>,
        context_key: Option<&str>,
        text: &'a str,
    ) -> Cow<'a, str> {
        if !self.enabled {
            return Cow::Borrowed(text);
        }
        let outcome = redact_with_audit(text);
        if outcome.hits.is_empty() {
            // Fast path: clean text → byte-identical, no allocation, no audit.
            return Cow::Borrowed(text);
        }
        if let Some(ref audit) = self.audit {
            audit(&sink.as_audit_label(), context_key, &outcome.hits);
        }
        Cow::Owned(outcome.text)
    }
}

impl Default for RedactionPolicy {
    fn default() -> Self {
        // A process that hasn't installed a policy must still be safe: a
        // missing policy MUST NOT mean "no redaction." Default to enabled
        // with no audit sink — the worst-case is missing audit rows, not
        // leaked secrets.
        Self::enabled_no_audit()
    }
}

static GLOBAL_POLICY: OnceLock<Mutex<RedactionPolicy>> = OnceLock::new();

/// Install the process-wide policy. Called once from daemon / TUI startup
/// after `Config` is loaded and the DB handle is available. Subsequent
/// calls replace the policy in place (live-toggle: a settings change
/// rebuilds and reinstalls). Returns the previous policy.
pub fn install_global(policy: RedactionPolicy) -> RedactionPolicy {
    let cell = GLOBAL_POLICY.get_or_init(|| Mutex::new(RedactionPolicy::default()));
    let mut guard = match cell.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!("redaction policy mutex was poisoned during install; recovering");
            poisoned.into_inner()
        }
    };
    std::mem::replace(&mut *guard, policy)
}

/// Snapshot the current global policy. Returns the safe default
/// ([`RedactionPolicy::enabled_no_audit`]) if nothing has been installed
/// yet — for example in early boot, in tests, or in CLI subcommands that
/// don't open the DB.
pub fn current_policy() -> RedactionPolicy {
    match GLOBAL_POLICY.get() {
        Some(m) => match m.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => {
                tracing::warn!("redaction policy mutex was poisoned during snapshot; recovering");
                poisoned.into_inner().clone()
            }
        },
        None => RedactionPolicy::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::SECRET_PATTERNS_FOR_TEST;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory audit sink — captures every recorded hit for assertions.
    #[derive(Default)]
    struct Recorder {
        rows: Mutex<Vec<(String, Option<String>, RedactionHit)>>,
    }
    impl Recorder {
        fn record_fn(self: Arc<Self>) -> Arc<RedactionAuditFn> {
            Arc::new(move |sink, ctx, hits| {
                let mut g = self.rows.lock().unwrap();
                for h in hits {
                    g.push((sink.to_string(), ctx.map(str::to_string), h.clone()));
                }
            })
        }
        fn rows(&self) -> Vec<(String, Option<String>, RedactionHit)> {
            self.rows.lock().unwrap().clone()
        }
        fn count(&self) -> usize {
            self.rows.lock().unwrap().len()
        }
    }

    fn enabled_policy() -> (RedactionPolicy, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        let policy = RedactionPolicy::with_audit(true, rec.clone().record_fn());
        (policy, rec)
    }

    #[test]
    fn disabled_policy_returns_borrowed_unchanged() {
        // Toggle-off contract: zero work, byte-identical, no audit row.
        let rec = Arc::new(Recorder::default());
        let policy = RedactionPolicy::with_audit(false, rec.clone().record_fn());
        let secret = "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let out = policy.apply(OutputSink::Channel("telegram"), Some("u:t"), secret);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(&*out, secret);
        assert_eq!(rec.count(), 0);
    }

    #[test]
    fn enabled_policy_redacts_and_invokes_audit() {
        let (policy, rec) = enabled_policy();
        let input = "creds: AKIAIOSFODNN7EXAMPLE end";
        let out = policy.apply(OutputSink::Channel("slack"), Some("user-1:thread-2"), input);
        assert!(matches!(out, Cow::Owned(_)));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(out.contains("[REDACTED AWS Access Key]"));

        let rows = rec.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "channel:slack");
        assert_eq!(rows[0].1.as_deref(), Some("user-1:thread-2"));
        assert_eq!(rows[0].2.label, "AWS Access Key");
        assert_eq!(rows[0].2.count, 1);
    }

    #[test]
    fn clean_text_is_borrowed_and_writes_no_audit() {
        // Fast path: enabled policy + benign text → zero allocation,
        // zero audit rows. A regression that started returning Cow::Owned
        // here would silently double-allocate every channel message.
        let (policy, rec) = enabled_policy();
        let input = "Hello world — nothing sensitive here.";
        let out = policy.apply(OutputSink::Tui, None, input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(&*out, input);
        assert_eq!(rec.count(), 0);
    }

    #[test]
    fn multi_pattern_payload_produces_one_row_per_pattern() {
        let (policy, rec) = enabled_policy();
        let input = concat!(
            "aws=AKIAIOSFODNN7EXAMPLE ",
            "gh=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk ",
            "jwt=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        );
        let out = policy.apply(OutputSink::Channel("discord"), Some("u:t"), input);
        assert!(out.contains("[REDACTED AWS Access Key]"));
        assert!(out.contains("[REDACTED GitHub Token]"));
        assert!(out.contains("[REDACTED JWT]"));
        let rows = rec.rows();
        let labels: Vec<&str> = rows.iter().map(|r| r.2.label).collect();
        assert!(labels.contains(&"AWS Access Key"));
        assert!(labels.contains(&"GitHub Token"));
        assert!(labels.contains(&"JWT"));
        for r in &rows {
            assert_eq!(r.1.as_deref(), Some("u:t"));
            assert_eq!(r.0, "channel:discord");
        }
    }

    #[test]
    fn repeat_pattern_collapses_to_one_hit_with_count() {
        // Two AWS keys in one payload → one audit hit, count=2 (not two
        // hits). Catches a regression that over-counts if the detector
        // started emitting per-match audit hits instead of per-pattern.
        let (policy, rec) = enabled_policy();
        let input = "k1=AKIAIOSFODNN7EXAMPLE k2=AKIA0000000000000001";
        policy.apply(OutputSink::Tui, Some("session-x"), input);
        let rows = rec.rows();
        let aws: Vec<&RedactionHit> = rows
            .iter()
            .map(|r| &r.2)
            .filter(|h| h.label == "AWS Access Key")
            .collect();
        assert_eq!(aws.len(), 1, "single audit row for the AWS pattern");
        assert_eq!(aws[0].count, 2, "hit count must reflect both matches");
    }

    #[test]
    fn double_redaction_is_noop() {
        // Output already containing "[REDACTED …]" markers must not be
        // re-rewritten, and must not generate fresh audit rows. Lets the
        // gateway redact once and the tee-log layer redact the same
        // string again safely.
        let (policy, rec) = enabled_policy();
        let once = policy.apply(
            OutputSink::Channel("telegram"),
            Some("u:t"),
            "k=AKIAIOSFODNN7EXAMPLE",
        );
        let before = rec.count();
        let twice = policy.apply(OutputSink::Channel("telegram"), Some("u:t"), &once);
        assert_eq!(&*twice, &*once);
        assert_eq!(rec.count(), before);
    }

    #[test]
    fn sink_audit_labels_match_specification() {
        // Stability contract: an operator's saved queries against
        // `redaction_audit.sink` must keep working across releases.
        assert_eq!(
            OutputSink::Channel("telegram").as_audit_label(),
            "channel:telegram"
        );
        assert_eq!(OutputSink::Tui.as_audit_label(), "tui");
        assert_eq!(OutputSink::Log.as_audit_label(), "log");
    }

    #[test]
    fn every_pattern_is_caught_through_the_policy() {
        // Iterates every pattern the input-side detector knows about and
        // proves the output-side policy also strips it. Catches the failure
        // mode where a new pattern is added in secrets.rs but a parallel
        // detector for output paths drifts behind. Reusing the same
        // pattern list is the DRY guarantee — this test pins it.
        let (policy, rec) = enabled_policy();
        for sample in SECRET_PATTERNS_FOR_TEST {
            let input = format!("prefix {sample} suffix");
            let out = policy.apply(OutputSink::Channel("slack"), Some("ctx"), &input);
            assert!(
                out.contains("[REDACTED"),
                "pattern not redacted on output side: sample={sample:?} got={out:?}",
            );
            assert!(
                !out.contains(sample),
                "raw secret leaked through policy: sample={sample:?}",
            );
        }
        // Every sample produced at least one hit.
        assert!(rec.count() >= SECRET_PATTERNS_FOR_TEST.len());
    }

    #[test]
    fn audit_closure_invoked_exactly_once_per_apply_with_hits() {
        // Pins the invocation contract: `apply()` calls the audit closure
        // exactly once per call that produced ≥1 hit, with the full hit
        // list batched — not once per hit. A regression that flipped this
        // to per-hit invocation would inflate DB write traffic and reshape
        // every operator query against `redaction_audit`.
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let policy = RedactionPolicy::with_audit(
            true,
            Arc::new(move |_, _, _| {
                calls2.fetch_add(1, Ordering::SeqCst);
            }),
        );
        // Single-hit payload → one closure call.
        let _ = policy.apply(OutputSink::Tui, None, "k=AKIAIOSFODNN7EXAMPLE");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Multi-hit payload → still one closure call (batched).
        let _ = policy.apply(
            OutputSink::Tui,
            None,
            "a=AKIAIOSFODNN7EXAMPLE b=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk",
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Clean payload → no additional call.
        let _ = policy.apply(OutputSink::Tui, None, "nothing to see");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn default_policy_is_safe() {
        // A process that hasn't installed a policy must still strip
        // secrets. The default is enabled-no-audit, NOT disabled —
        // otherwise the global-install pattern could leak secrets during
        // early boot or in test harnesses that never install one.
        let p = RedactionPolicy::default();
        assert!(p.is_enabled());
        let out = p.apply(OutputSink::Log, None, "k=AKIAIOSFODNN7EXAMPLE");
        assert!(out.contains("[REDACTED AWS Access Key]"));
    }
}
