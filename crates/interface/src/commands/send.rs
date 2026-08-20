//! `borg send` — hand a running session a new user turn from outside it.
//!
//! The daemon is long-lived and its sessions outlive any one client, but until
//! now the only way to give one a message was to be the client attached to it.
//! This is the seam for everything that is not a person at a TUI: a cron line,
//! a git hook, a second terminal, a script that finished a long build and wants
//! the agent to look at the log.
//!
//! No new RPC: the turn goes through the same `Session.Send` the TUI uses, so
//! it lands in history, bills the same budget, and reaches every surface
//! subscribed to the session's event stream (the web and desktop apps consume
//! exactly that stream, so a message sent here shows up live in an open
//! browser tab).

use anyhow::{bail, Context, Result};

use protocols::session::agent_event::Kind;

/// One candidate session, reduced to the fields target resolution needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionCandidate {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) updated_unix_ts: u64,
    /// The daemon currently hosts this session, so `Session.Send` can reach it.
    pub(crate) hosted: bool,
}

/// The suffix the daemon appends to a hosted session's title when it lists it.
/// It is display text, not part of the name anyone typed, so it has to come
/// back off before a title is compared — and it is also the only signal in the
/// listing that says which rows `Session.Send` can actually reach.
const HOSTED_TITLE_SUFFIX: &str = " (live)";

impl SessionCandidate {
    /// Build a candidate from one `Admin.ListSessions` row.
    ///
    /// Kept separate from the RPC type so the suffix handling is testable
    /// against titles spelled exactly the way the daemon renders them.
    fn from_listing(id: String, listed_title: &str, updated_unix_ts: u64) -> Self {
        match listed_title.strip_suffix(HOSTED_TITLE_SUFFIX) {
            Some(title) => Self {
                id,
                title: title.to_string(),
                updated_unix_ts,
                hosted: true,
            },
            None => Self {
                id,
                title: listed_title.to_string(),
                updated_unix_ts,
                hosted: false,
            },
        }
    }
}

/// Why a target could not be turned into exactly one session.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// Nothing matched by id prefix or title.
    NotFound,
    /// An id prefix matched more than one session. Unlike a repeated title,
    /// this is the caller mistyping an identifier rather than a name they
    /// legitimately reused, so guessing would be wrong.
    AmbiguousPrefix(Vec<String>),
}

/// Resolve a user-supplied target to one session id.
///
/// Order is id-prefix first, then exact title, then case-insensitive title: an
/// id is unambiguous by construction, so a title that happens to look like a
/// hex prefix must not shadow the real session.
///
/// Duplicate *titles* resolve to a hosted match first and the most recently
/// updated one second, rather than erroring. Titles are generated from the
/// first message, so repeats are ordinary — and the session someone means by a
/// name they used twice is the one this command can actually reach, or failing
/// that the one they touched last.
pub(crate) fn resolve_target(
    candidates: &[SessionCandidate],
    target: &str,
) -> Result<String, ResolveError> {
    let target = target.trim();
    if target.is_empty() {
        return Err(ResolveError::NotFound);
    }

    let by_prefix: Vec<&SessionCandidate> = candidates
        .iter()
        .filter(|c| c.id.starts_with(target))
        .collect();
    match by_prefix.as_slice() {
        [only] => return Ok(only.id.clone()),
        [] => {}
        many => {
            return Err(ResolveError::AmbiguousPrefix(
                many.iter().map(|c| c.id.clone()).collect(),
            ))
        }
    }

    // Hosted first, then recency. `borg send` can only deliver to a session
    // the daemon still holds open, so preferring a newer *closed* row over the
    // running one it shares a name with would turn a working command into the
    // "nobody has it open" error for no reason.
    let best = |mut matches: Vec<&SessionCandidate>| -> Option<String> {
        matches.sort_by_key(|c| (c.hosted, c.updated_unix_ts));
        matches.last().map(|c| c.id.clone())
    };

    if let Some(id) = best(candidates.iter().filter(|c| c.title == target).collect()) {
        return Ok(id);
    }
    best(
        candidates
            .iter()
            .filter(|c| c.title.eq_ignore_ascii_case(target))
            .collect(),
    )
    .ok_or(ResolveError::NotFound)
}

/// Render a resolution failure as something the operator can act on.
fn describe(target: &str, err: ResolveError) -> anyhow::Error {
    match err {
        ResolveError::NotFound => anyhow::anyhow!(
            "no live session matches '{target}' — run `borg sessions list` to see what is running"
        ),
        ResolveError::AmbiguousPrefix(ids) => {
            let shown: Vec<String> = ids
                .iter()
                .map(|id| crate::commands::short_id(id).to_string())
                .collect();
            anyhow::anyhow!(
                "'{target}' matches {} sessions ({}) — use more of the id",
                ids.len(),
                shown.join(", ")
            )
        }
    }
}

/// True when a `Session.Send` failure means "that session exists but nobody
/// has it open" rather than a transport or daemon problem.
///
/// Matched on the gRPC status rather than the message text: the daemon answers
/// `NotFound` only for an unhosted session id here, and the id has already been
/// resolved against the stored set, so it cannot be a typo by this point.
fn is_not_hosted(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tonic::Status>()
            .is_some_and(|status| status.code() == tonic::Code::NotFound)
    })
}

/// Replace the raw gRPC status with the next action, in plain words.
fn not_running_hint(session_id: &str, target: &str, err: anyhow::Error) -> anyhow::Error {
    if is_not_hosted(&err) {
        let short = crate::commands::short_id(session_id);
        return anyhow::anyhow!(
            "'{target}' is a stored session ({short}) that nobody has open — \
             `borg send` talks to running sessions. Open it with \
             `borg --resume {short}`, then send again."
        );
    }
    err.context(format!(
        "sending to {}",
        crate::commands::short_id(session_id)
    ))
}

/// Echo one streamed chunk, flushed so a long turn appears as it arrives
/// rather than all at once when the process exits.
///
/// Returns the I/O error instead of panicking the way `print!` does: this
/// command is aimed at scripts, and `borg send … | head -1` closes the pipe on
/// purpose. Dying with "failed printing to stdout" there would be wrong.
fn echo(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// Send `message` to the session named by `target`.
///
/// Blocks until the turn finishes. The stream is what keeps the turn alive
/// from this client's point of view, and draining it is also what lets a
/// failure surface here instead of only in the daemon log.
pub(crate) async fn run(target: &str, message: &str) -> Result<()> {
    let message = message.trim();
    if message.is_empty() {
        bail!("nothing to send — provide a message");
    }

    let mut client = crate::daemon_client::connect_default().await?;
    let list = client
        .list_sessions()
        .await
        .context("Admin.ListSessions via borgd")?;
    let candidates: Vec<SessionCandidate> = list
        .sessions
        .into_iter()
        .map(|s| SessionCandidate::from_listing(s.id, &s.title, s.updated_unix_ts))
        .collect();
    let session_id = resolve_target(&candidates, target).map_err(|e| describe(target, e))?;

    // `Session.Send` needs a *hosted* session, but the listing above is the
    // stored set — most rows in it are conversations nobody currently has
    // open. Rather than auto-hosting (which would have to guess a workspace,
    // and a wrong guess silently runs the agent in the wrong tree), say so.
    let mut stream = client
        .send_message(&session_id, message)
        .await
        .map_err(|e| not_running_hint(&session_id, target, e))?;

    // Echo the reply so `borg send` is usable on its own from a script, not
    // only as a way to poke a session someone else is watching.
    let mut saw_error = None;
    let mut wrote_any = false;
    while let Some(event) = stream.message().await.context("session event stream")? {
        match event.kind {
            Some(Kind::TextDelta(delta)) if !delta.text.is_empty() => {
                if echo(&delta.text).is_err() {
                    // Whoever was reading stopped (`borg send … | head -1`).
                    // That is an ordinary way to stop reading, not a failure
                    // of the turn, so stop echoing and let the session finish.
                    break;
                }
                wrote_any = true;
            }
            Some(Kind::Error(error)) => {
                saw_error = Some(error.message);
                break;
            }
            Some(Kind::TurnComplete(_)) => break,
            _ => {}
        }
    }
    if wrote_any {
        // Best effort: the turn already happened, and a closed pipe must not
        // turn a delivered message into a failure exit code.
        let _ = echo("\n");
    }

    if let Some(error) = saw_error {
        bail!("the session reported an error: {error}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed session, as the listing renders one.
    fn candidate(id: &str, title: &str, updated: u64) -> SessionCandidate {
        SessionCandidate::from_listing(id.to_string(), title, updated)
    }

    /// A session the daemon is hosting, spelled the way `Admin.ListSessions`
    /// actually renders it.
    fn hosted(id: &str, title: &str, updated: u64) -> SessionCandidate {
        SessionCandidate::from_listing(id.to_string(), &format!("{title} (live)"), updated)
    }

    #[test]
    fn an_id_prefix_wins_over_a_title_that_looks_like_one() {
        // A generated title can start with the same characters as somebody
        // else's session id. Ids are unambiguous by construction, so resolving
        // titles first would let a stray title hijack a message meant for a
        // specific session.
        let candidates = vec![
            candidate("abc123", "some work", 10),
            candidate("def456", "abc123", 99),
        ];
        assert_eq!(resolve_target(&candidates, "abc123"), Ok("abc123".into()));
    }

    #[test]
    fn a_title_matches_the_session_the_daemon_reports_as_live() {
        // The listing decorates a hosted session's title with a " (live)"
        // marker for display. Comparing against the decorated string means the
        // title arms can only ever match sessions nobody has open — which are
        // precisely the ones `borg send` cannot deliver to. So the one target
        // this command exists to serve would be unreachable by name.
        let candidates = vec![hosted("aaa", "deploy check", 10)];
        assert_eq!(
            resolve_target(&candidates, "deploy check"),
            Ok("aaa".into()),
            "a live session must match the title its user actually typed"
        );
        assert_eq!(
            candidates[0].title, "deploy check",
            "the display marker is not part of the name"
        );
        assert!(candidates[0].hosted);
    }

    #[test]
    fn a_live_session_beats_a_newer_closed_one_sharing_its_title() {
        // Recency is the tie-break *within* what is reachable, not above it.
        // Picking the newer closed row would answer a perfectly good
        // `borg send` with "nobody has it open" while the session the user
        // meant sits there running.
        let candidates = vec![
            hosted("live", "deploy check", 100),
            candidate("dead", "deploy check", 900),
        ];
        assert_eq!(
            resolve_target(&candidates, "deploy check"),
            Ok("live".into())
        );
    }

    #[test]
    fn a_repeated_title_resolves_to_the_most_recent_session() {
        // Titles come from the first message, so repeats are ordinary rather
        // than a mistake. Refusing them would make `borg send "deploy"` fail
        // for exactly the people who use it most; the one they mean is the one
        // they touched last.
        let candidates = vec![
            candidate("aaa", "deploy check", 100),
            candidate("bbb", "deploy check", 300),
            candidate("ccc", "deploy check", 200),
        ];
        assert_eq!(
            resolve_target(&candidates, "deploy check"),
            Ok("bbb".into())
        );
    }

    #[test]
    fn an_ambiguous_id_prefix_is_refused_rather_than_guessed() {
        // The opposite call from titles: a short prefix matching two ids is a
        // typo, and picking the newest would send someone's message into the
        // wrong conversation with no sign anything went wrong.
        let candidates = vec![
            candidate("abc111", "one", 1),
            candidate("abc222", "two", 500),
        ];
        let err =
            resolve_target(&candidates, "abc").expect_err("ambiguous prefix must not resolve");
        match err {
            ResolveError::AmbiguousPrefix(ids) => assert_eq!(ids.len(), 2),
            other => panic!("expected an ambiguity error, got {other:?}"),
        }
    }

    #[test]
    fn an_exact_title_beats_a_case_insensitive_one() {
        // Case-insensitive matching is a convenience for typing, not a licence
        // to ignore an exact match that is right there.
        let candidates = vec![
            candidate("aaa", "Deploy", 10),
            candidate("bbb", "deploy", 5),
        ];
        assert_eq!(resolve_target(&candidates, "deploy"), Ok("bbb".into()));
        assert_eq!(resolve_target(&candidates, "DEPLOY"), Ok("aaa".into()));
    }

    #[test]
    fn nothing_matching_reports_not_found() {
        let candidates = vec![candidate("aaa", "one", 1)];
        assert_eq!(
            resolve_target(&candidates, "zzz"),
            Err(ResolveError::NotFound)
        );
        assert_eq!(
            resolve_target(&candidates, "   "),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn an_unhosted_session_is_explained_rather_than_reported_as_grpc_notfound() {
        // The id resolved fine — it is a real session — so the raw
        // `status: NotFound` a caller would otherwise see is actively
        // misleading: it reads as "no such session" when the session exists
        // and merely is not open. Borg's UX invariant wants the next action.
        let status = tonic::Status::not_found("session `abc123-...` not found");
        let err = anyhow::Error::new(status).context("Session.Send failed");
        assert!(
            is_not_hosted(&err),
            "a NotFound anywhere in the chain counts"
        );

        let message = not_running_hint("abc12345-6789", "my-session", err).to_string();
        assert!(
            message.contains("borg --resume abc12345"),
            "must name the command that opens it: {message}"
        );
        assert!(
            !message.contains("NotFound"),
            "the gRPC status must not leak into operator-facing text: {message}"
        );
    }

    #[test]
    fn a_transport_failure_is_not_mistaken_for_an_unhosted_session() {
        // The control: telling the operator to `--resume` would send them
        // chasing the wrong thing when the daemon is simply unreachable, so
        // anything that is not a NotFound must keep its original cause.
        let status = tonic::Status::unavailable("connection refused");
        let err = anyhow::Error::new(status);
        assert!(!is_not_hosted(&err));

        let message = not_running_hint("abc12345-6789", "my-session", err).to_string();
        assert!(
            !message.contains("--resume"),
            "an unreachable daemon is not a closed session: {message}"
        );
    }

    #[test]
    fn resolution_failures_name_the_next_step() {
        // Borg's UX invariant: an error says what to do, in plain words.
        let not_found = describe("ghost", ResolveError::NotFound).to_string();
        assert!(
            not_found.contains("borg sessions list"),
            "must point at the way to see live sessions: {not_found}"
        );
        let ambiguous = describe(
            "abc",
            ResolveError::AmbiguousPrefix(vec!["abc1".into(), "abc2".into()]),
        )
        .to_string();
        assert!(
            ambiguous.contains("use more of the id"),
            "must say how to disambiguate: {ambiguous}"
        );
    }
}
