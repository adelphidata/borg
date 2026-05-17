//! Loader for user-defined slash commands at `~/.borg/commands/<name>.md`.
//!
//! Files are markdown with optional YAML frontmatter (`---` fenced block at
//! the top). The body is a prompt template; on submission, `$ARGUMENTS` is
//! substituted with whatever the user typed after the command name.
//!
//! Built-in commands always win on name collisions — this is intentional so
//! a stray `~/.borg/commands/help.md` cannot shadow `/help`.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub template: String,
}

/// Load every `*.md` file under `dir` as a custom command. Missing or
/// unreadable directory → empty vec; per-file parse errors → skip with warn.
/// Result is sorted by name for stable popup ordering.
pub fn load_from_dir(dir: &Path) -> Vec<CustomCommand> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() && is_valid_command_name(s) => s.to_string(),
            _ => {
                tracing::warn!(path = %path.display(), "skipping custom command: invalid file name");
                continue;
            }
        };
        let body = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "skipping custom command: read failed");
                continue;
            }
        };
        let (description, template) = parse_frontmatter(&body);
        out.push(CustomCommand {
            name: format!("/{stem}"),
            description: description.unwrap_or_else(|| "(user command)".to_string()),
            template,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Names must be ASCII alphanumerics, `-`, or `_`. Mirrors what the popup
/// already filters on so loaded commands are reachable via the search box.
fn is_valid_command_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Split optional `---`-fenced YAML frontmatter from the markdown body.
/// Recognized keys: `description`. Anything else in the frontmatter is
/// ignored. If parsing fails or there is no frontmatter, returns the whole
/// body as the template and `None` for the description.
fn parse_frontmatter(input: &str) -> (Option<String>, String) {
    let trimmed = input.trim_start_matches('\u{FEFF}');
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (None, input.to_string());
    };
    // The opening `---` must be on its own line.
    let after_open = match rest.strip_prefix('\n') {
        Some(s) => s,
        None => return (None, input.to_string()),
    };
    // Find the closing `---` on its own line.
    let Some(end_idx) = find_line(after_open, "---") else {
        return (None, input.to_string());
    };
    let header = &after_open[..end_idx];
    let body_start = end_idx + "---".len();
    let body = after_open[body_start..]
        .strip_prefix('\n')
        .unwrap_or(&after_open[body_start..]);

    let mut description = None;
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("description:") {
            description = Some(strip_quotes(value.trim()).to_string());
        }
    }
    (description, body.to_string())
}

/// Find the byte offset of the first occurrence of `needle` that starts at
/// the beginning of a line. Returns `None` if no such line exists.
fn find_line(haystack: &str, needle: &str) -> Option<usize> {
    let mut offset = 0;
    for line in haystack.split_inclusive('\n') {
        let trimmed_end = line.trim_end_matches('\n');
        if trimmed_end == needle {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0];
        let last = bytes[s.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Substitute `$ARGUMENTS` with the supplied text. If the template does not
/// reference `$ARGUMENTS` and `args` is non-empty, append `args` on a new
/// line so the user's text is still passed to the model.
pub fn expand_template(template: &str, args: &str) -> String {
    if template.contains("$ARGUMENTS") {
        return template.replace("$ARGUMENTS", args);
    }
    if args.is_empty() {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len() + args.len() + 2);
    out.push_str(template.trim_end_matches('\n'));
    out.push('\n');
    out.push_str(args);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn loads_simple_command_without_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "standup.md", "What did I do today?\n");
        let cmds = load_from_dir(dir.path());
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "/standup");
        assert_eq!(cmds[0].description, "(user command)");
        assert_eq!(cmds[0].template, "What did I do today?\n");
    }

    #[test]
    fn loads_command_with_frontmatter_description() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "review.md",
            "---\ndescription: Code review template\n---\nReview this: $ARGUMENTS\n",
        );
        let cmds = load_from_dir(dir.path());
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "/review");
        assert_eq!(cmds[0].description, "Code review template");
        assert_eq!(cmds[0].template, "Review this: $ARGUMENTS\n");
    }

    #[test]
    fn frontmatter_quoted_description_is_unquoted() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "q.md",
            "---\ndescription: \"with: colon\"\n---\nbody\n",
        );
        let cmds = load_from_dir(dir.path());
        assert_eq!(cmds[0].description, "with: colon");
    }

    #[test]
    fn frontmatter_with_unknown_keys_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "x.md",
            "---\ndescription: yes\nargument-hint: <topic>\nmodel: opus\n---\nbody\n",
        );
        let cmds = load_from_dir(dir.path());
        assert_eq!(cmds[0].description, "yes");
        assert_eq!(cmds[0].template, "body\n");
    }

    #[test]
    fn malformed_frontmatter_is_treated_as_body() {
        // No closing `---` → fall back to using the entire input as the body.
        let dir = tempfile::tempdir().unwrap();
        let raw = "---\ndescription: oops\nbody continues without close\n";
        write(dir.path(), "broken.md", raw);
        let cmds = load_from_dir(dir.path());
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].description, "(user command)");
        assert_eq!(cmds[0].template, raw);
    }

    #[test]
    fn ignores_non_md_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "notes.txt", "ignored");
        write(dir.path(), "real.md", "kept");
        let cmds = load_from_dir(dir.path());
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "/real");
    }

    #[test]
    fn skips_invalid_filenames() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bad name.md", "x"); // space rejected
        write(dir.path(), "good_name.md", "y");
        let cmds = load_from_dir(dir.path());
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "/good_name");
    }

    #[test]
    fn missing_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let nope = dir.path().join("does-not-exist");
        assert!(load_from_dir(&nope).is_empty());
    }

    #[test]
    fn results_are_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "zeta.md", "z");
        write(dir.path(), "alpha.md", "a");
        write(dir.path(), "mid.md", "m");
        let cmds = load_from_dir(dir.path());
        let names: Vec<_> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/alpha", "/mid", "/zeta"]);
    }

    /// `$ARGUMENTS` is substituted wherever it appears (including multiple
    /// times); when the template has no placeholder, the args are appended
    /// (so the user's words still reach the prompt). Empty args collapse
    /// the placeholder to "" rather than leaving the literal text.
    #[test]
    fn expand_template_substitution_rules() {
        let cases: &[(&str, &str, &str)] = &[
            // Placeholder present.
            ("Review: $ARGUMENTS please", "src/foo.rs", "Review: src/foo.rs please"),
            // Placeholder absent, args present → appended verbatim.
            ("Do the thing.\n", "extra context", "Do the thing.\nextra context"),
            // No placeholder, no args → template returned verbatim.
            ("Just this.", "", "Just this."),
            // Placeholder + empty args → placeholder collapses to "".
            ("Hi $ARGUMENTS!", "", "Hi !"),
            // Multiple occurrences all substituted.
            ("$ARGUMENTS and $ARGUMENTS", "X", "X and X"),
        ];
        for (tpl, args, expected) in cases {
            assert_eq!(
                expand_template(tpl, args),
                *expected,
                "template={tpl:?} args={args:?}"
            );
        }
    }
}
