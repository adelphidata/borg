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
pub(crate) mod send;
pub(crate) mod sessions;
pub(crate) mod settings;
pub(crate) mod skills;
pub(crate) mod status;
pub(crate) mod subscriptions;
pub(crate) mod tasks;
pub(crate) mod tunnel;
pub(crate) mod update_args;
pub(crate) mod wasm_plugin;
pub(crate) mod web;
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

/// The id to print for `id` in a listing of `all`: its first 8 characters,
/// lengthened until no other listed id shares them, so the printed id always
/// resolves through [`resolve_unique_prefix`]. Built-in rows share a long
/// common prefix (`00000000-0000-4000-8000-…`), where a flat 8-character cut
/// prints the same unusable id on every row.
pub(crate) fn listed_id<'a>(id: &'a str, all: &[&str]) -> &'a str {
    let mut len = 8.min(id.len());
    while len < id.len()
        && all
            .iter()
            .any(|other| *other != id && other.starts_with(&id[..len]))
    {
        len += 1;
    }
    &id[..len]
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

/// Print `lines` to stdout, one per line.
///
/// Long output is often piped into a reader that stops early (`borg logs |
/// head`). The closed pipe ends the output, not the command: `println!`
/// panics on it. Every other write error is returned.
///
/// Make it the command's last write to stdout: after a closed pipe it
/// returns `Ok`, and a later `println!` would still panic.
pub(crate) fn print_lines<T: std::fmt::Display>(
    lines: impl IntoIterator<Item = T>,
) -> anyhow::Result<()> {
    write_lines(&mut std::io::stdout().lock(), lines)
}

fn write_lines<T: std::fmt::Display>(
    out: &mut impl std::io::Write,
    lines: impl IntoIterator<Item = T>,
) -> anyhow::Result<()> {
    let written = lines
        .into_iter()
        .try_for_each(|line| writeln!(out, "{line}"))
        .and_then(|()| out.flush());
    match written {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            tracing::debug!("stdout reader closed the pipe; output stopped early");
            Ok(())
        }
        other => anyhow::Context::context(other, "could not write to stdout"),
    }
}

#[cfg(test)]
mod print_lines_tests {
    use super::write_lines;
    use std::io::{self, ErrorKind, Write};

    /// Takes `room` bytes, then fails each write with `kind`. With
    /// `fail_flush` set, the flush fails with `kind` too.
    struct ClosingWriter {
        written: Vec<u8>,
        room: usize,
        fail_flush: bool,
        kind: ErrorKind,
    }

    impl Write for ClosingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.written.len() >= self.room {
                return Err(self.kind.into());
            }
            let n = buf.len().min(self.room - self.written.len());
            self.written.extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(self.kind.into())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_closed_pipe_ends_the_output_and_other_errors_fail() {
        for (room, fail_flush, kind, ok) in [
            // The reader left in the middle of the output.
            (8, false, ErrorKind::BrokenPipe, true),
            // The reader left before the last flush.
            (usize::MAX, true, ErrorKind::BrokenPipe, true),
            // A full disk under `borg logs > file` is a real failure.
            (8, false, ErrorKind::StorageFull, false),
            (usize::MAX, true, ErrorKind::StorageFull, false),
        ] {
            let mut out = ClosingWriter {
                written: Vec::new(),
                room,
                fail_flush,
                kind,
            };
            let result = write_lines(&mut out, ["first line", "second line"]);
            assert_eq!(
                result.is_ok(),
                ok,
                "room {room}, fail_flush {fail_flush}, {kind:?}: {result:?}"
            );
            if !ok {
                let err = result.expect_err("checked above");
                let io_err = err.downcast_ref::<io::Error>().expect("an io error");
                assert_eq!(io_err.kind(), kind);
            }
        }
    }

    #[test]
    fn every_line_is_written_in_order() {
        let mut out = Vec::new();
        write_lines(&mut out, ["first line", "second line"]).expect("write to a vec");
        assert_eq!(out, b"first line\nsecond line\n");
    }
}

#[cfg(test)]
mod resolve_prefix_tests {
    use super::{listed_id, resolve_unique_prefix};

    /// Every printed id must resolve back to its own row, and a random-UUID
    /// row keeps the usual 8-character id.
    #[test]
    fn listed_ids_resolve_back_to_their_own_row() {
        let all = [
            "00000000-0000-4000-8000-5ec041700001",
            "00000000-0000-4000-8000-5ec041700002",
            "00000000-0000-4000-8000-da1175000002",
            "3f2a9c1e-7b44-4c1a-9e0d-2b6f1c8a7d10",
        ];
        let printed: Vec<&str> = all.iter().map(|id| listed_id(id, &all)).collect();
        assert_eq!(printed[3], "3f2a9c1e");
        assert_eq!(printed[2], "00000000-0000-4000-8000-d");
        for (id, shown) in all.iter().zip(&printed) {
            let pool = all.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                resolve_unique_prefix("tasks", shown, pool)
                    .unwrap()
                    .as_deref(),
                Some(*id),
                "printed id {shown} must resolve to {id}"
            );
        }
    }

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
