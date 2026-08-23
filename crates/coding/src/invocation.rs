//! Detect `apply_patch` invocations embedded in shell-command strings.
//!
//! Some LLM tool-call shapes emit the patch as a `run_shell` command rather
//! than the dedicated `apply_patch` tool, e.g.:
//!
//! ```text
//! bash -lc "apply_patch <<'EOF'
//! *** Begin Patch
//! *** Add File: foo.txt
//! +hi
//! *** End Patch
//! EOF"
//! ```
//!
//! Running these as raw shell fails (the `apply_patch` binary isn't on
//! `$PATH`) and bypasses borg's verified patch path (vitals classification,
//! blocked-path enforcement, ghost-commit undo). The functions here recognize
//! the heredoc wrappers and pull out the patch body so the shell handler can
//! reroute to `handle_apply_patch`.
//!
//! Deliberately hand-rolled rather than tree-sitter: the surface we accept is
//! bounded (`bash -lc`, `bash -c`, `sh -c`, or a bare command starting with
//! `apply_patch`), and pulling tree-sitter into the binary just for this
//! would dwarf the actual logic.
use std::borrow::Cow;

/// If `command` is recognizably an `apply_patch <<HEREDOC ... HEREDOC` shell
/// invocation (optionally wrapped in `bash -lc` / `bash -c` / `sh -c`, and
/// optionally prefixed with `cd <dir> &&`), return the heredoc body — i.e. the
/// patch text — so the caller can dispatch through the verified patch path.
///
/// Returns `None` for any input that isn't unambiguously an `apply_patch`
/// heredoc, including:
/// * commands without `apply_patch`,
/// * `apply_patch` with arguments instead of a heredoc,
/// * heredocs whose closing tag doesn't appear on its own line,
/// * unbalanced quoting that the scanner can't resolve safely.
pub fn maybe_extract_apply_patch_from_shell(command: &str) -> Option<String> {
    let unwrapped = unwrap_shell_wrapper(command.trim());
    let stripped = strip_leading_cd(unwrapped.trim());
    extract_heredoc_body(stripped)
}

/// Peel off `bash -lc <arg>`, `bash -c <arg>`, or `sh -c <arg>`, returning the
/// inner argument with one layer of shell quoting removed. If the input isn't
/// in that form, return it unchanged.
fn unwrap_shell_wrapper(command: &str) -> Cow<'_, str> {
    for prefix in ["bash -lc ", "bash -c ", "sh -c ", "zsh -c "] {
        if let Some(rest) = command.strip_prefix(prefix) {
            if let Some(inner) = strip_one_layer_of_quotes(rest.trim()) {
                return Cow::Owned(inner);
            }
            return Cow::Borrowed(rest);
        }
    }
    Cow::Borrowed(command)
}

/// Remove a single matched pair of outer single-quotes or double-quotes.
/// Returns `None` if the input is not single-quote- or double-quote-wrapped.
/// Inside double quotes, unescape `\"` and `\\` (a minimal subset — borg
/// doesn't need to emulate the full bash escape grammar to recognize the
/// patch heredoc).
fn strip_one_layer_of_quotes(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return None;
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if first != last {
        return None;
    }
    match first {
        b'\'' => Some(s[1..s.len() - 1].to_string()),
        b'"' => {
            let inner = &s[1..s.len() - 1];
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(next) = chars.next() {
                        match next {
                            '"' | '\\' | '$' | '`' => out.push(next),
                            other => {
                                out.push('\\');
                                out.push(other);
                            }
                        }
                    } else {
                        out.push('\\');
                    }
                } else {
                    out.push(c);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// `cd /some/where && <rest>` is a common idiom; we don't honor the `cd` (the
/// caller's cwd is authoritative for the patch), but we do strip it so the
/// remainder can be matched as `apply_patch <<...`.
fn strip_leading_cd(s: &str) -> &str {
    let lowered = s.trim_start();
    if let Some(rest) = lowered.strip_prefix("cd ") {
        if let Some(idx) = rest.find("&&") {
            return rest[idx + 2..].trim_start();
        }
    }
    s
}

/// Match `apply_patch <<[-]TAG\n<body>\nTAG` (with TAG optionally quoted).
/// The `apply_patch` token must be at the start (after optional whitespace)
/// and followed by the heredoc opener — bare invocations like
/// `apply_patch foo.patch` or `cat | apply_patch` return `None`.
fn extract_heredoc_body(s: &str) -> Option<String> {
    let trimmed = s.trim_start();
    let after = trimmed.strip_prefix("apply_patch")?;
    // Require whitespace after the command name to avoid matching tokens like
    // `apply_patchwrong`.
    if !after.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let after = after.trim_start();
    let after = after.strip_prefix("<<")?;
    let after = after.strip_prefix('-').unwrap_or(after);

    // Tag may be 'EOF', "EOF", or bare EOF.
    let (tag, after_tag) = parse_heredoc_tag(after)?;
    // Skip to end of opener line.
    let body_start = after_tag.find('\n')? + 1;
    let body_and_rest = &after_tag[body_start..];

    // Find the closing tag on its own line. A trailing line of just the tag
    // (with optional whitespace) terminates the body. The `\n<tag>` form is
    // the common case; we also accept `<tag>` at byte 0 if the body starts
    // with the terminator (empty body — uncommon but harmless).
    let needle_nl = format!("\n{tag}");
    if let Some(idx) = body_and_rest.find(&needle_nl) {
        // Validate that the line containing the tag ends cleanly (newline,
        // whitespace, or EOF) so we don't false-match `EOF_NEXT` when the
        // tag is `EOF`.
        let tail = &body_and_rest[idx + needle_nl.len()..];
        let valid_terminator = tail.is_empty()
            || tail.starts_with('\n')
            || tail.chars().next().is_some_and(char::is_whitespace);
        if valid_terminator {
            return Some(body_and_rest[..idx].to_string());
        }
    }
    None
}

/// Parse a heredoc tag: `'TAG'`, `"TAG"`, or bare `TAG`. Returns the tag and
/// the remainder of the input after the tag.
fn parse_heredoc_tag(s: &str) -> Option<(String, &str)> {
    if let Some(rest) = s.strip_prefix('\'') {
        let end = rest.find('\'')?;
        Some((rest[..end].to_string(), &rest[end + 1..]))
    } else if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        Some((rest[..end].to_string(), &rest[end + 1..]))
    } else {
        let end = s
            .find(|c: char| c.is_whitespace() || c == '\n')
            .unwrap_or(s.len());
        if end == 0 {
            return None;
        }
        Some((s[..end].to_string(), &s[end..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH_BODY: &str = "*** Begin Patch\n*** Add File: hi.txt\n+hi\n*** End Patch";

    #[test]
    fn extracts_bare_apply_patch_heredoc() {
        let cmd = format!("apply_patch <<'EOF'\n{PATCH_BODY}\nEOF");
        let body = maybe_extract_apply_patch_from_shell(&cmd).expect("should extract");
        assert_eq!(body, PATCH_BODY);
    }

    #[test]
    fn extracts_bash_lc_double_quoted() {
        let cmd = format!("bash -lc \"apply_patch <<'EOF'\n{PATCH_BODY}\nEOF\"");
        let body = maybe_extract_apply_patch_from_shell(&cmd).expect("should extract");
        assert_eq!(body, PATCH_BODY);
    }

    #[test]
    fn extracts_bash_c_single_quoted() {
        let cmd = format!("bash -c 'apply_patch <<EOF\n{PATCH_BODY}\nEOF'");
        let body = maybe_extract_apply_patch_from_shell(&cmd).expect("should extract");
        assert_eq!(body, PATCH_BODY);
    }

    #[test]
    fn extracts_sh_c_with_double_quoted_tag() {
        let cmd = format!("sh -c 'apply_patch <<\"EOF\"\n{PATCH_BODY}\nEOF'");
        let body = maybe_extract_apply_patch_from_shell(&cmd).expect("should extract");
        assert_eq!(body, PATCH_BODY);
    }

    #[test]
    fn extracts_with_leading_cd() {
        let cmd = format!("cd /tmp && apply_patch <<'PATCH'\n{PATCH_BODY}\nPATCH");
        let body = maybe_extract_apply_patch_from_shell(&cmd).expect("should extract");
        assert_eq!(body, PATCH_BODY);
    }

    #[test]
    fn handles_dash_heredoc_form() {
        let cmd = format!("apply_patch <<-EOF\n{PATCH_BODY}\nEOF");
        let body = maybe_extract_apply_patch_from_shell(&cmd).expect("should extract");
        assert_eq!(body, PATCH_BODY);
    }

    #[test]
    fn returns_none_for_non_apply_patch_command() {
        assert!(maybe_extract_apply_patch_from_shell("ls -la").is_none());
        assert!(maybe_extract_apply_patch_from_shell("echo hello").is_none());
    }

    #[test]
    fn returns_none_for_apply_patch_with_arg_instead_of_heredoc() {
        assert!(maybe_extract_apply_patch_from_shell("apply_patch foo.patch").is_none());
        assert!(maybe_extract_apply_patch_from_shell("cat foo | apply_patch").is_none());
    }

    #[test]
    fn returns_none_for_command_starting_with_similar_token() {
        assert!(maybe_extract_apply_patch_from_shell("apply_patchwrong <<EOF\nx\nEOF").is_none());
    }

    #[test]
    fn returns_none_when_heredoc_terminator_missing() {
        let cmd = "apply_patch <<'EOF'\n*** Begin Patch\n*** End Patch\n";
        assert!(maybe_extract_apply_patch_from_shell(cmd).is_none());
    }

    #[test]
    fn returns_none_when_heredoc_tag_appears_only_inside_body() {
        // The body contains "EOFISH" but no standalone "EOF" line.
        let cmd = "apply_patch <<EOF\nEOFISH\n";
        assert!(maybe_extract_apply_patch_from_shell(cmd).is_none());
    }

    #[test]
    fn extracts_with_trailing_whitespace_after_closing_tag() {
        // A closing-tag line with a trailing space, followed by further
        // command lines, must still terminate the heredoc — non-newline
        // whitespace after the tag is a valid terminator.
        let cmd = format!("apply_patch <<EOF\n{PATCH_BODY}\nEOF \necho done");
        let body = maybe_extract_apply_patch_from_shell(&cmd)
            .expect("a trailing space after the closing tag must still terminate");
        assert_eq!(body, PATCH_BODY);
    }

    #[test]
    fn extracts_with_space_between_opening_tag_and_newline() {
        // A space after the opening tag must not become part of the tag —
        // the closing line is bare `EOF` and must still match.
        let cmd = format!("apply_patch <<EOF \n{PATCH_BODY}\nEOF");
        let body = maybe_extract_apply_patch_from_shell(&cmd)
            .expect("whitespace ends a bare heredoc tag");
        assert_eq!(body, PATCH_BODY);
    }

    #[test]
    fn strip_one_layer_of_quotes_length_edge_cases() {
        // Inputs shorter than one full quote pair can never be unwrapped and
        // must not panic; an empty quote pair unwraps to the empty string.
        assert_eq!(strip_one_layer_of_quotes(""), None);
        assert_eq!(strip_one_layer_of_quotes("'"), None);
        assert_eq!(strip_one_layer_of_quotes("''"), Some(String::new()));
        assert_eq!(strip_one_layer_of_quotes("\"\""), Some(String::new()));
    }

    #[test]
    fn parse_heredoc_tag_returns_remainder_directly_after_tag() {
        // The remainder must start immediately after the tag (and after the
        // closing quote for quoted tags) — not on the quote itself.
        assert_eq!(
            parse_heredoc_tag("'EOF' rest"),
            Some(("EOF".to_string(), " rest"))
        );
        assert_eq!(
            parse_heredoc_tag("\"TAG\"\nbody"),
            Some(("TAG".to_string(), "\nbody"))
        );
        // Bare tags end at the first whitespace of any kind, not only '\n'.
        assert_eq!(
            parse_heredoc_tag("EOF more"),
            Some(("EOF".to_string(), " more"))
        );
    }
}
