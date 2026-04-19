//! Patch DSL parser — converts patch text into structured operations.

use thiserror::Error;

/// Errors that can occur during patch parsing.
#[derive(Debug, Error, PartialEq, Clone)]
pub enum ParseError {
    /// The overall patch structure is malformed.
    #[error("Invalid patch format: {0}")]
    InvalidFormat(String),
    /// A specific hunk within the patch is invalid.
    #[error("Invalid hunk at line {line_number}: {message}")]
    InvalidHunk {
        /// Description of what went wrong.
        message: String,
        /// Line number in the patch text where the error occurred.
        line_number: usize,
    },
}

/// A single operation within a patch.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchOperation {
    /// Create a new file with the given content.
    AddFile {
        /// Relative path for the new file.
        path: String,
        /// File content (lines joined with newlines).
        content: String,
    },
    /// Modify an existing file by applying hunks.
    UpdateFile {
        /// Relative path of the file to update.
        path: String,
        /// Optional new path to move the file to after patching.
        move_to: Option<String>,
        /// Ordered list of search-and-replace hunks.
        hunks: Vec<Hunk>,
    },
    /// Remove a file from the filesystem.
    DeleteFile {
        /// Relative path of the file to delete.
        path: String,
    },
}

/// A single search-and-replace block within an `UpdateFile` operation.
#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    /// Optional `@@` context hint (e.g., function name) for fuzzy matching.
    pub context_hint: Option<String>,
    /// Text to search for in the file.
    pub search: String,
    /// Replacement text.
    pub replace: String,
    /// Whether this hunk targets the end of the file.
    pub is_end_of_file: bool,
    /// Line number in the patch text where this hunk starts.
    pub source_line: usize,
}

/// A parsed patch containing one or more file operations.
#[derive(Debug, PartialEq)]
pub struct Patch {
    /// Ordered list of operations to apply.
    pub operations: Vec<PatchOperation>,
}

// ── Marker constants ──

const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";

/// Check if a line starts with a diff prefix (` `, `+`, or `-`).
fn is_diff_prefix(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b' ' | b'+' | b'-'))
}

/// Check if `text` is a DSL marker that should never appear as file content.
/// Used to detect when an LLM erroneously prefixes a marker with `+` or `-`.
fn is_dsl_marker(text: &str) -> bool {
    let t = text.trim();
    t == END_PATCH_MARKER
        || t == BEGIN_PATCH_MARKER
        || t.starts_with(ADD_FILE_MARKER)
        || t.starts_with(UPDATE_FILE_MARKER)
        || t.starts_with(DELETE_FILE_MARKER)
        || t.starts_with(MOVE_TO_MARKER)
        || t == EOF_MARKER
}

/// Classification of a single line inside a diff hunk.
enum DiffLine<'a> {
    /// ` ` context prefix (or an empty line, which acts as empty context).
    Context(&'a str),
    /// `+` added line, stripped of the prefix.
    Add(&'a str),
    /// `-` removed line, stripped of the prefix.
    Remove(&'a str),
    /// `*** End of File` sentinel.
    EndOfFile,
    /// Anything else: the caller must stop reading diff lines.
    Stop,
}

/// Classify a single diff line without doing any semantic checks.
fn classify_diff_line(line: &str) -> DiffLine<'_> {
    if line.trim() == EOF_MARKER {
        return DiffLine::EndOfFile;
    }
    if let Some(removed) = line.strip_prefix('-') {
        return DiffLine::Remove(removed);
    }
    if let Some(added) = line.strip_prefix('+') {
        return DiffLine::Add(added);
    }
    if let Some(context) = line.strip_prefix(' ') {
        return DiffLine::Context(context);
    }
    if line.is_empty() {
        return DiffLine::Context(line);
    }
    DiffLine::Stop
}

/// LLM-error guard: returns `true` when `payload` looks like a DSL marker
/// (e.g. `*** End Patch`) and no further diff line follows — a strong hint
/// that the model erroneously prefixed the marker with `+`/`-`, and we
/// should treat the current line as end-of-hunk rather than content.
fn is_hijacked_marker(payload: &str, lines: &[&str], next_idx: usize) -> bool {
    if !is_dsl_marker(payload) {
        return false;
    }
    let next_is_diff = next_idx < lines.len() && is_diff_prefix(lines[next_idx]);
    !next_is_diff
}

/// Collect diff lines starting at `lines[*i]`. Each line must start with
/// ` ` (context), `+` (add), or `-` (remove). Empty lines are treated as
/// empty context. Stops at a non-diff line or `*** End of File` marker.
/// Returns `(search_lines, replace_lines, is_end_of_file)`.
fn collect_diff_lines<'a>(lines: &[&'a str], i: &mut usize) -> (Vec<&'a str>, Vec<&'a str>, bool) {
    let mut search_lines = Vec::new();
    let mut replace_lines = Vec::new();
    let mut is_end_of_file = false;

    while *i < lines.len() {
        match classify_diff_line(lines[*i]) {
            DiffLine::EndOfFile => {
                is_end_of_file = true;
                *i += 1;
                break;
            }
            DiffLine::Remove(removed) => {
                if is_hijacked_marker(removed, lines, *i + 1) {
                    *i += 1;
                    break;
                }
                search_lines.push(removed);
            }
            DiffLine::Add(added) => {
                if is_hijacked_marker(added, lines, *i + 1) {
                    *i += 1;
                    break;
                }
                replace_lines.push(added);
            }
            DiffLine::Context(ctx) => {
                search_lines.push(ctx);
                replace_lines.push(ctx);
            }
            DiffLine::Stop => break,
        }
        *i += 1;
    }

    (search_lines, replace_lines, is_end_of_file)
}

/// Strip heredoc wrapping (`<<'EOF'...EOF`) that LLMs sometimes produce.
/// Supports any delimiter word (EOF, PATCH, END, etc.) with optional single or double quotes.
/// Returns the original lines unchanged if no valid heredoc wrapper is detected.
fn strip_heredoc_wrapper(lines: Vec<&str>) -> Vec<&str> {
    if lines.len() < 4 {
        return lines;
    }
    let first = lines.first().map(|l| l.trim()).unwrap_or("");
    let last = lines.last().map(|l| l.trim()).unwrap_or("");
    if let Some(rest) = first.strip_prefix("<<") {
        // Strip matching quotes ('' or ""), reject mismatched quotes
        let delim = if let Some(inner) = rest.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))
        {
            inner
        } else if let Some(inner) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            inner
        } else {
            rest
        };
        if !delim.is_empty() && last == delim {
            return lines[1..lines.len() - 1].to_vec();
        }
    }
    lines
}

/// Parse a patch string into a structured `Patch`.
///
/// The format follows the codex apply-patch DSL:
///
/// ```text
/// *** Begin Patch
/// *** Add File: path
/// +line1
/// +line2
/// *** Update File: path
/// @@ optional context
///  context line
/// -removed line
/// +added line
/// *** Delete File: path
/// *** End Patch
/// ```
///
/// Key rules:
/// - `*** Begin Patch` and `*** End Patch` are required boundaries.
/// - Add File content lines MUST start with `+` (stripped when stored).
///   Lines without `+` terminate the add block — this prevents ambiguity
///   when file content itself contains `***` markers.
/// - Update File hunk lines use unified diff prefixes: ` ` (context),
///   `-` (remove), `+` (add). Lines without these prefixes terminate the hunk.
/// - Delete File has no body.
pub fn parse_patch(input: &str) -> Result<Patch, ParseError> {
    let lines: Vec<&str> = input.lines().collect();

    // Heredoc tolerance: strip <<'EOF'...EOF wrapping that LLMs sometimes emit.
    let lines = strip_heredoc_wrapper(lines);

    let mut operations = Vec::new();
    let mut i = 0;

    // Skip leading whitespace/empty lines
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }

    // Expect "*** Begin Patch" (optional — be lenient for backward compat)
    if i < lines.len() && lines[i].trim() == BEGIN_PATCH_MARKER {
        i += 1;
    }

    while i < lines.len() {
        let line = lines[i].trim();

        if line == END_PATCH_MARKER || line.is_empty() {
            i += 1;
            continue;
        }

        if let Some(path) = line.strip_prefix(ADD_FILE_MARKER) {
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(ParseError::InvalidFormat(
                    "Empty file path in Add File".to_string(),
                ));
            }
            i += 1;

            // Collect content: every line MUST start with '+'.
            // Lines without '+' terminate the block (they belong to the
            // next operation or are the *** End Patch marker).
            // If a '+'-prefixed line is a DSL marker (e.g. +*** End Patch)
            // and no more '+' lines follow, it's an LLM error — strip it.
            let mut content = String::new();
            while i < lines.len() {
                if let Some(added) = lines[i].strip_prefix('+') {
                    if is_dsl_marker(added) {
                        let next_is_plus = i + 1 < lines.len() && lines[i + 1].starts_with('+');
                        if !next_is_plus {
                            i += 1;
                            break;
                        }
                    }
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(added);
                    i += 1;
                } else {
                    break;
                }
            }
            operations.push(PatchOperation::AddFile { path, content });
        } else if let Some(path) = line.strip_prefix(UPDATE_FILE_MARKER) {
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(ParseError::InvalidFormat(
                    "Empty file path in Update File".to_string(),
                ));
            }
            let update_line_number = i + 1; // 1-based, for error messages
            i += 1;

            // Check for optional *** Move to: line
            let move_to = if i < lines.len() {
                let next = lines[i].trim();
                if let Some(dest) = next.strip_prefix(MOVE_TO_MARKER) {
                    i += 1;
                    Some(dest.trim().to_string())
                } else {
                    None
                }
            } else {
                None
            };

            let mut hunks = Vec::new();

            // Parse chunks until we hit a *** marker (next operation or end)
            while i < lines.len() && !lines[i].starts_with("***") {
                let hunk_line = lines[i].trim();

                // Skip blank lines between chunks
                if hunk_line.is_empty() {
                    i += 1;
                    continue;
                }

                if let Some(after_at) = hunk_line.strip_prefix("@@") {
                    let hunk_source_line = i + 1; // 1-based line number

                    // Extract context hint from @@ header (strip trailing @@ if present)
                    let context_hint = {
                        let trimmed = after_at.trim();
                        let trimmed = trimmed.strip_suffix("@@").unwrap_or(trimmed).trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    };

                    i += 1;
                    let (search_lines, replace_lines, is_end_of_file) =
                        collect_diff_lines(&lines, &mut i);

                    hunks.push(Hunk {
                        context_hint,
                        search: search_lines.join("\n"),
                        replace: replace_lines.join("\n"),
                        is_end_of_file,
                        source_line: hunk_source_line,
                    });
                } else if hunks.is_empty() && is_diff_prefix(lines[i]) {
                    // First chunk without @@ header — allow diff lines directly
                    // (LLMs sometimes omit the @@ marker on the first chunk)
                    let hunk_source_line = i + 1;
                    let (search_lines, replace_lines, is_end_of_file) =
                        collect_diff_lines(&lines, &mut i);

                    hunks.push(Hunk {
                        context_hint: None,
                        search: search_lines.join("\n"),
                        replace: replace_lines.join("\n"),
                        is_end_of_file,
                        source_line: hunk_source_line,
                    });
                } else {
                    // Non-@@ non-blank line — terminates the UpdateFile block
                    break;
                }
            }

            // Reject empty UpdateFile (no hunks and no move) — likely a malformed patch
            if hunks.is_empty() && move_to.is_none() {
                return Err(ParseError::InvalidHunk {
                    message: format!("Update file hunk for path '{path}' is empty"),
                    line_number: update_line_number,
                });
            }

            operations.push(PatchOperation::UpdateFile {
                path,
                move_to,
                hunks,
            });
        } else if let Some(path) = line.strip_prefix(DELETE_FILE_MARKER) {
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(ParseError::InvalidFormat(
                    "Empty file path in Delete File".to_string(),
                ));
            }
            operations.push(PatchOperation::DeleteFile { path });
            i += 1;
        } else {
            i += 1;
        }
    }

    if operations.is_empty() {
        return Err(ParseError::InvalidFormat(
            "No operations found in patch".to_string(),
        ));
    }

    Ok(Patch { operations })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Add File ──

    #[test]
    fn parse_add_file() {
        let input = "\
*** Begin Patch
*** Add File: src/new.rs
+fn main() {}
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "src/new.rs");
                assert_eq!(content, "fn main() {}");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_add_file_multiline_content() {
        let input = "\
*** Begin Patch
*** Add File: src/main.rs
+fn main() {
+    println!(\"hello\");
+    println!(\"world\");
+}
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { content, .. } => {
                assert!(content.contains("println!(\"hello\");"));
                assert!(content.contains("println!(\"world\");"));
                assert_eq!(content.lines().count(), 4);
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_add_file_content_with_embedded_patch_markers() {
        // Content that contains "*** " text is safe because those lines
        // will have '+' prefix. Lines without '+' terminate the block.
        let input = "\
*** Begin Patch
*** Add File: README.md
+# My Tool
+
+## Patch DSL
+
+Use the following format:
+
+*** Begin Patch
+*** Add File: tool-name/tool.toml
+content here
+*** End Patch
+
+That's how you create tools.
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(
            patch.operations.len(),
            1,
            "Should produce exactly one AddFile, got: {:?}",
            patch.operations
        );
        match &patch.operations[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "README.md");
                assert!(
                    content.contains("*** Begin Patch"),
                    "Content should preserve embedded *** Begin Patch"
                );
                assert!(
                    content.contains("*** Add File: tool-name/tool.toml"),
                    "Content should preserve embedded *** Add File"
                );
                assert!(
                    content.contains("That's how you create tools."),
                    "Content should include text after embedded patch example"
                );
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_add_file_content_with_markdown_bold() {
        let input = "\
*** Begin Patch
*** Add File: notes.md
+# Important
+
+***Note:*** This is critical.
+*** Bold header ***
+Some more text.
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "notes.md");
                assert!(content.contains("***Note:***"));
                assert!(content.contains("*** Bold header ***"));
                assert!(content.contains("Some more text."));
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_add_file_without_begin_end_markers() {
        // The parser is lenient and doesn't require Begin/End markers
        let input = "\
*** Add File: test.txt
+content here";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "test.txt");
                assert_eq!(content, "content here");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    // ── Delete File ──

    #[test]
    fn parse_delete_file() {
        let input = "\
*** Begin Patch
*** Delete File: src/old.rs
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::DeleteFile { path } => {
                assert_eq!(path, "src/old.rs");
            }
            other => panic!("Expected DeleteFile, got {:?}", other),
        }
    }

    // ── Update File ──

    #[test]
    fn parse_update_file_with_hunks() {
        let input = "\
*** Begin Patch
*** Update File: src/lib.rs
@@ fn hello()
 fn hello() {
-    println!(\"hi\");
+    println!(\"hello world\");
 }
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::UpdateFile { path, hunks, .. } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(hunks.len(), 1);
                assert!(hunks[0].search.contains("println!(\"hi\");"));
                assert!(hunks[0].replace.contains("println!(\"hello world\");"));
                assert!(hunks[0].search.contains("fn hello()"));
                assert!(hunks[0].replace.contains("fn hello()"));
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_update_with_multiple_hunks() {
        let input = "\
*** Begin Patch
*** Update File: lib.rs
@@
-fn old_a() {}
+fn new_a() {}
@@
-fn old_b() {}
+fn new_b() {}
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks.len(), 2);
                assert!(hunks[0].search.contains("old_a"));
                assert!(hunks[0].replace.contains("new_a"));
                assert!(hunks[1].search.contains("old_b"));
                assert!(hunks[1].replace.contains("new_b"));
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_update_with_context_lines() {
        let input = "\
*** Begin Patch
*** Update File: main.rs
@@
 use std::io;
 fn main() {
-    old_call();
+    new_call();
 }
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks.len(), 1);
                assert!(hunks[0].search.contains("use std::io;"));
                assert!(hunks[0].replace.contains("use std::io;"));
                assert!(hunks[0].search.contains("old_call()"));
                assert!(!hunks[0].search.contains("new_call()"));
                assert!(hunks[0].replace.contains("new_call()"));
                assert!(!hunks[0].replace.contains("old_call()"));
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_update_followed_by_add() {
        // Ensures *** marker correctly terminates an update block
        let input = "\
*** Begin Patch
*** Update File: file.py
@@
+line
*** Add File: other.py
+content
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 2);
        match &patch.operations[0] {
            PatchOperation::UpdateFile { path, hunks, .. } => {
                assert_eq!(path, "file.py");
                assert_eq!(hunks.len(), 1);
                assert!(hunks[0].replace.contains("line"));
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
        match &patch.operations[1] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "other.py");
                assert_eq!(content, "content");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    // ── Multiple operations ──

    #[test]
    fn parse_multiple_operations() {
        let input = "\
*** Begin Patch
*** Add File: a.txt
+hello
*** Delete File: b.txt
*** Update File: c.txt
@@
-old
+new
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 3);
        assert!(matches!(
            &patch.operations[0],
            PatchOperation::AddFile { .. }
        ));
        assert!(matches!(
            &patch.operations[1],
            PatchOperation::DeleteFile { .. }
        ));
        assert!(matches!(
            &patch.operations[2],
            PatchOperation::UpdateFile { .. }
        ));
    }

    #[test]
    fn parse_full_example() {
        // Mirrors the codex documentation example
        let input = "\
*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
@@ def greet():
-print(\"Hi\")
+print(\"Hello, world!\")
*** Delete File: obsolete.txt
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 3);

        match &patch.operations[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "hello.txt");
                assert_eq!(content, "Hello world");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
        match &patch.operations[1] {
            PatchOperation::UpdateFile { path, hunks, .. } => {
                assert_eq!(path, "src/app.py");
                assert_eq!(hunks.len(), 1);
                assert!(hunks[0].search.contains("print(\"Hi\")"));
                assert!(hunks[0].replace.contains("print(\"Hello, world!\")"));
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
        match &patch.operations[2] {
            PatchOperation::DeleteFile { path } => {
                assert_eq!(path, "obsolete.txt");
            }
            other => panic!("Expected DeleteFile, got {:?}", other),
        }
    }

    // ── Error cases ──

    #[test]
    fn parse_empty_input_errors() {
        let result = parse_patch("");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("No operations found"), "got: {err}");
    }

    #[test]
    fn parse_garbage_input_errors() {
        let result = parse_patch("this is not a patch at all");
        assert!(result.is_err());
    }

    // ── Edge cases ──

    #[test]
    fn parse_add_file_empty_content() {
        // Add file with no + lines produces empty content
        let input = "\
*** Begin Patch
*** Add File: empty.txt
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::AddFile { content, .. } => {
                assert_eq!(content, "");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_update_empty_hunk_header() {
        // @@ with no context text is valid
        let input = "\
*** Begin Patch
*** Update File: test.rs
@@
-old
+new
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].search, "old");
                assert_eq!(hunks[0].replace, "new");
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_whitespace_padded_markers() {
        // Markers with leading/trailing whitespace should still be recognized
        let input = "  *** Begin Patch  \n*** Add File: f.txt\n+hi\n  *** End Patch  ";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "f.txt");
                assert_eq!(content, "hi");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    // ── New feature tests ──

    #[test]
    fn parse_move_to() {
        let input = "\
*** Begin Patch
*** Update File: old.rs
*** Move to: new.rs
@@
-old
+new
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile {
                path,
                move_to,
                hunks,
                ..
            } => {
                assert_eq!(path, "old.rs");
                assert_eq!(move_to.as_deref(), Some("new.rs"));
                assert_eq!(hunks.len(), 1);
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_end_of_file_marker() {
        let input = "\
*** Begin Patch
*** Update File: f.txt
@@
-old
+new
*** End of File
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks.len(), 1);
                assert!(hunks[0].is_end_of_file);
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_context_hint() {
        let input = "\
*** Begin Patch
*** Update File: f.txt
@@ fn hello()
 fn hello() {
-    old();
+    new();
 }
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks[0].context_hint.as_deref(), Some("fn hello()"));
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_source_line_tracking() {
        let input = "\
*** Begin Patch
*** Update File: f.txt
@@
-old
+new
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                // @@ is on line 3 (1-indexed)
                assert_eq!(hunks[0].source_line, 3);
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_no_context_hint_gives_none() {
        let input = "\
*** Begin Patch
*** Update File: f.txt
@@
-old
+new
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert!(hunks[0].context_hint.is_none());
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_no_eof_marker_gives_false() {
        let input = "\
*** Begin Patch
*** Update File: f.txt
@@
-old
+new
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert!(!hunks[0].is_end_of_file);
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_update_without_move_to() {
        let input = "\
*** Begin Patch
*** Update File: f.txt
@@
-old
+new
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { move_to, .. } => {
                assert!(move_to.is_none());
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    // ── Heredoc tolerance ──

    #[test]
    fn parse_heredoc_wrapped_patch() {
        let input = "<<'EOF'\n\
*** Begin Patch\n\
*** Add File: test.txt\n\
+hello\n\
*** End Patch\n\
EOF";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "test.txt");
                assert_eq!(content, "hello");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_heredoc_unquoted() {
        let input = "<<EOF\n\
*** Begin Patch\n\
*** Add File: test.txt\n\
+hello\n\
*** End Patch\n\
EOF";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
    }

    #[test]
    fn parse_heredoc_double_quoted() {
        let input = "<<\"EOF\"\n\
*** Begin Patch\n\
*** Add File: test.txt\n\
+hello\n\
*** End Patch\n\
EOF";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
    }

    #[test]
    fn parse_heredoc_mismatched_not_stripped() {
        // Mismatched quotes should NOT trigger heredoc stripping.
        // The inner patch content may still parse due to Borg's lenient parser,
        // but the heredoc lines themselves are not consumed as valid operations.
        // Use content that would fail without proper stripping: no Begin/End inside.
        let input = "<<\"EOF'\n\
+hello\n\
EOF";
        let result = parse_patch(input);
        // Without heredoc stripping, "<<\"EOF'" and "EOF" are not valid operations,
        // and "+hello" alone is not recognized outside an AddFile block.
        assert!(result.is_err());
    }

    // ── First chunk without @@ header ──

    #[test]
    fn parse_first_chunk_without_at_header() {
        let input = "\
*** Begin Patch
*** Update File: file.py
 import foo
+bar
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks.len(), 1);
                assert!(hunks[0].context_hint.is_none());
                assert!(hunks[0].search.contains("import foo"));
                assert!(hunks[0].replace.contains("import foo"));
                assert!(hunks[0].replace.contains("bar"));
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_second_chunk_requires_at_header() {
        // Second chunk without @@ should terminate the UpdateFile, not be parsed as diff
        let input = "\
*** Begin Patch
*** Update File: file.py
@@
-old
+new
not_a_diff_line
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(
                    hunks.len(),
                    1,
                    "second non-@@ block should not create a hunk"
                );
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    // ── Empty update rejection ──

    #[test]
    fn parse_empty_update_rejected() {
        let input = "\
*** Begin Patch
*** Update File: test.py
*** End Patch";
        let result = parse_patch(input);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("empty"),
            "expected 'empty' in error, got: {err}"
        );
    }

    #[test]
    fn parse_move_only_allowed() {
        let input = "\
*** Begin Patch
*** Update File: old.txt
*** Move to: new.txt
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile {
                path,
                move_to,
                hunks,
            } => {
                assert_eq!(path, "old.txt");
                assert_eq!(move_to.as_deref(), Some("new.txt"));
                assert!(hunks.is_empty());
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_heredoc_with_eof_in_content() {
        // Content that itself contains "EOF" should not confuse heredoc stripping
        let input = "<<'EOF'\n*** Begin Patch\n*** Add File: t.txt\n+EOF\n*** End Patch\nEOF";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::AddFile { content, .. } => assert_eq!(content, "EOF"),
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_heredoc_custom_delimiter() {
        // LLMs sometimes use PATCH or END instead of EOF
        let input = "<<'PATCH'\n*** Begin Patch\n*** Add File: t.txt\n+hi\n*** End Patch\nPATCH";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
    }

    #[test]
    fn parse_first_chunk_pure_deletion_without_at() {
        let input = "\
*** Begin Patch
*** Update File: file.py
-old_line
-another_old
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].search, "old_line\nanother_old");
                assert!(hunks[0].replace.is_empty());
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    // ── DSL marker stripping (LLM error recovery) ──

    #[test]
    fn parse_add_file_strips_plus_end_patch_marker() {
        // LLM erroneously prefixes *** End Patch with +
        let input = "\
*** Begin Patch
*** Add File: src/new.rs
+fn main() {}
+*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "src/new.rs");
                assert_eq!(content, "fn main() {}");
                assert!(
                    !content.contains("End Patch"),
                    "DSL marker should not leak into content, got: {content}"
                );
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_add_file_plus_end_patch_followed_by_more_ops() {
        // +*** End Patch is the last '+' line, so it's stripped.
        // Subsequent operations are parsed normally.
        let input = "\
*** Begin Patch
*** Add File: a.txt
+hello
+*** End Patch
*** Delete File: b.txt
*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 2);
        match &patch.operations[0] {
            PatchOperation::AddFile { content, .. } => {
                assert_eq!(content, "hello");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
        assert!(matches!(
            &patch.operations[1],
            PatchOperation::DeleteFile { .. }
        ));
    }

    #[test]
    fn parse_update_file_strips_plus_end_patch_marker() {
        let input = "\
*** Begin Patch
*** Update File: src/lib.rs
@@
-old_line
+new_line
+*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].search, "old_line");
                assert_eq!(hunks[0].replace, "new_line");
                assert!(
                    !hunks[0].replace.contains("End Patch"),
                    "DSL marker should not leak into replace text"
                );
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_update_file_preserves_embedded_end_patch_mid_content() {
        // +*** End Patch followed by more '+' lines is legitimate content
        let input = "\
*** Begin Patch
*** Update File: doc.md
@@
-old
+*** End Patch
+more content after
*** End Patch";
        let patch = parse_patch(input).unwrap();
        match &patch.operations[0] {
            PatchOperation::UpdateFile { hunks, .. } => {
                assert_eq!(hunks.len(), 1);
                assert!(
                    hunks[0].replace.contains("*** End Patch"),
                    "Embedded marker followed by more '+' lines should be preserved"
                );
                assert!(hunks[0].replace.contains("more content after"));
            }
            other => panic!("Expected UpdateFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_add_file_strips_plus_begin_patch_marker() {
        // LLM wraps content with both +*** Begin Patch and +*** End Patch
        let input = "\
*** Begin Patch
*** Add File: script.py
+*** Begin Patch
+print('hello')
+*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { content, .. } => {
                // +*** Begin Patch is followed by more + lines, so it's kept
                assert!(content.contains("*** Begin Patch"));
                assert!(content.contains("print('hello')"));
                // +*** End Patch is the last + line, so it's stripped
                assert!(
                    !content.contains("*** End Patch"),
                    "Trailing End Patch marker leaked: {content}"
                );
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_add_file_strips_all_dsl_markers_at_end() {
        // +*** Add File: as last line should be stripped too
        let input = "\
*** Begin Patch
*** Add File: test.py
+code here
+*** Add File: bogus.py";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { content, .. } => {
                assert_eq!(content, "code here");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }

    #[test]
    fn parse_add_file_no_content_only_plus_end_patch() {
        // Edge case: only line is +*** End Patch — should produce empty content
        let input = "\
*** Begin Patch
*** Add File: empty.py
+*** End Patch";
        let patch = parse_patch(input).unwrap();
        assert_eq!(patch.operations.len(), 1);
        match &patch.operations[0] {
            PatchOperation::AddFile { content, .. } => {
                assert_eq!(content, "");
            }
            other => panic!("Expected AddFile, got {:?}", other),
        }
    }
}
