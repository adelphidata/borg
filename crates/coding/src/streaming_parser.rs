//! Streaming patch parser.
//!
//! Parses the `*** Begin Patch / *** End Patch` DSL incrementally as bytes
//! arrive (e.g. from an LLM streaming the `apply_patch` tool-call argument).
//! Calls to [`StreamingPatchParser::push_delta`] return the current set of
//! parsed hunks each time a complete line is consumed, so a UI can render
//! progress before the model has finished emitting the call.
//!
//! Uses parallel [`StreamHunk`] / [`StreamUpdateChunk`] types because the
//! incremental shape is line-based (`Vec<String>`), whereas the batch
//! `parser::Hunk` joins lines into single `search` / `replace` strings.
//! Once the patch is complete, convert with [`to_patch_operations`] to
//! produce the same AST that the batch parser yields, ready for
//! `apply::apply_patch`.
//!
//! TUI integration (rendering events) is intentionally out of scope here;
//! this module just exposes the engine.
//!
//! ## Known divergence from the batch parser
//!
//! In `AddFile` mode, a content line of the form `+*** End Patch` (i.e. the
//! LLM is adding a file that *legitimately contains the DSL end marker as
//! data*) terminates the patch instead of being recorded as content.
//! Header detection runs before the `+` strip, which is acceptable
//! because real-world AddFile bodies essentially
//! never contain DSL sentinels. The batch parser at `parser.rs` has a
//! lookahead-based workaround; the streaming parser cannot replicate it
//! without buffering an entire trailing block, which would defeat the
//! purpose of streaming.

use std::path::PathBuf;

use crate::parser::{
    Hunk, ParseError, PatchOperation, ADD_FILE_MARKER, BEGIN_PATCH_MARKER, CHANGE_CONTEXT_MARKER,
    DELETE_FILE_MARKER, EMPTY_CHANGE_CONTEXT_MARKER, END_PATCH_MARKER, EOF_MARKER, MOVE_TO_MARKER,
    UPDATE_FILE_MARKER,
};

/// One file-level operation produced by the streaming parser.
///
/// Exposed as a sibling type to `parser::PatchOperation` because the streaming
/// representation accumulates `old_lines` / `new_lines` as `Vec<String>`
/// rather than the joined-string form used by the batch parser. Use
/// [`to_patch_operations`] to convert once a patch is complete.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamHunk {
    /// Create a new file with the given content.
    AddFile {
        /// Relative path for the new file.
        path: PathBuf,
        /// File content accumulated so far (lines joined with `\n`, with a
        /// trailing `\n` after each `+` line).
        contents: String,
    },
    /// Remove a file from the filesystem.
    DeleteFile {
        /// Relative path of the file to delete.
        path: PathBuf,
    },
    /// Modify an existing file with one or more chunks.
    UpdateFile {
        /// Relative path of the file to update.
        path: PathBuf,
        /// Optional rename destination.
        move_path: Option<PathBuf>,
        /// Chunks parsed so far (in order).
        chunks: Vec<StreamUpdateChunk>,
    },
}

/// One `@@`-delimited chunk inside a `StreamHunk::UpdateFile`.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamUpdateChunk {
    /// Optional `@@ context` hint for matching.
    pub change_context: Option<String>,
    /// Lines to remove (` ` and `-` prefixed).
    pub old_lines: Vec<String>,
    /// Lines to insert (` ` and `+` prefixed).
    pub new_lines: Vec<String>,
    /// `*** End of File` sentinel was seen — anchor at end of file.
    pub is_end_of_file: bool,
}

/// Convenience event alias for callers that want a single "what just changed"
/// type. Reserved for future progress-rendering wire-up; today the engine
/// always returns the full hunk vector from [`StreamingPatchParser::push_delta`].
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A new top-level hunk just started (file granularity).
    HunkStarted(StreamHunk),
    /// The entire patch was terminated by `*** End Patch`.
    Completed,
}

/// Convert streaming hunks to the batch parser's `PatchOperation` AST so the
/// existing `apply::apply_patch` codepath can consume them unchanged.
///
/// `Move File` (with explicit `src -> dst` syntax) is not produced by the
/// streaming parser — the DSL uses `Update File` + `Move to:` instead, and
/// this function returns the same shape (`UpdateFile` with `move_to: Some`).
pub fn to_patch_operations(hunks: &[StreamHunk]) -> Vec<PatchOperation> {
    hunks
        .iter()
        .map(|h| match h {
            StreamHunk::AddFile { path, contents } => PatchOperation::AddFile {
                path: path.to_string_lossy().into_owned(),
                content: contents.trim_end_matches('\n').to_string(),
            },
            StreamHunk::DeleteFile { path } => PatchOperation::DeleteFile {
                path: path.to_string_lossy().into_owned(),
            },
            StreamHunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => PatchOperation::UpdateFile {
                path: path.to_string_lossy().into_owned(),
                move_to: move_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                hunks: chunks
                    .iter()
                    .map(|c| Hunk {
                        context_anchors: c.change_context.clone().into_iter().collect(),
                        search: c.old_lines.join("\n"),
                        replace: c.new_lines.join("\n"),
                        is_end_of_file: c.is_end_of_file,
                        // Streaming parser doesn't track a meaningful 1-based
                        // source line for individual chunks; preserve 0 as a
                        // sentinel and let downstream errors fall back to the
                        // file path for context.
                        source_line: 0,
                    })
                    .collect(),
            },
        })
        .collect()
}

/// Incremental parser for `*** Begin Patch` / `*** End Patch` blocks.
///
/// Feed bytes via [`Self::push_delta`] and call [`Self::finish`] once the
/// stream is closed. Both methods return the parser's current view of the
/// accumulated hunks so a UI can render progress.
#[derive(Debug, Default, Clone)]
pub struct StreamingPatchParser {
    line_buffer: String,
    state: StreamingParserState,
    line_number: usize,
}

#[derive(Debug, Default, Clone)]
struct StreamingParserState {
    mode: StreamingParserMode,
    hunks: Vec<StreamHunk>,
}

#[derive(Debug, Default, Clone)]
enum StreamingParserMode {
    #[default]
    NotStarted,
    StartedPatch,
    AddFile,
    DeleteFile,
    UpdateFile {
        hunk_line_number: usize,
    },
    EndedPatch,
}

impl StreamingPatchParser {
    /// Feed a chunk of patch text. Returns the current hunk vector (a clone
    /// of internal state) so the caller can rebroadcast progress.
    pub fn push_delta(&mut self, delta: &str) -> Result<Vec<StreamHunk>, ParseError> {
        for ch in delta.chars() {
            if ch == '\n' {
                let mut line = std::mem::take(&mut self.line_buffer);
                let stripped = line.strip_suffix('\r').map_or(line.len(), str::len);
                line.truncate(stripped);
                self.line_number += 1;
                self.process_line(&line)?;
            } else {
                self.line_buffer.push(ch);
            }
        }
        Ok(self.state.hunks.clone())
    }

    /// Signal end-of-stream. Flushes any remaining buffered line and verifies
    /// that `*** End Patch` was seen. Returns the final hunk vector.
    pub fn finish(&mut self) -> Result<Vec<StreamHunk>, ParseError> {
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.line_number += 1;
            if line.trim() == END_PATCH_MARKER {
                self.ensure_update_hunk_is_not_empty(line.trim())?;
                self.state.mode = StreamingParserMode::EndedPatch;
            } else {
                self.process_line(&line)?;
            }
        }

        if !matches!(self.state.mode, StreamingParserMode::EndedPatch) {
            return Err(ParseError::InvalidFormat(
                "The last line of the patch must be '*** End Patch'".to_string(),
            ));
        }
        Ok(self.state.hunks.clone())
    }

    fn ensure_update_hunk_is_not_empty(&self, line: &str) -> Result<(), ParseError> {
        let Some(StreamHunk::UpdateFile { path, chunks, .. }) = self.state.hunks.last() else {
            return Ok(());
        };
        if chunks.is_empty() {
            if let StreamingParserMode::UpdateFile { hunk_line_number } = self.state.mode {
                return Err(ParseError::InvalidHunk {
                    message: format!("Update file hunk for path '{}' is empty", path.display()),
                    line_number: hunk_line_number,
                });
            }
        }
        if chunks
            .last()
            .is_some_and(|c| c.old_lines.is_empty() && c.new_lines.is_empty())
        {
            if line == END_PATCH_MARKER {
                return Err(ParseError::InvalidHunk {
                    message: "Update hunk does not contain any lines".to_string(),
                    line_number: self.line_number,
                });
            }
            return Err(ParseError::InvalidHunk {
                message: format!(
                    "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                ),
                line_number: self.line_number,
            });
        }
        Ok(())
    }

    fn handle_hunk_headers_and_end_patch(&mut self, trimmed: &str) -> Result<bool, ParseError> {
        if trimmed == END_PATCH_MARKER {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.mode = StreamingParserMode::EndedPatch;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(StreamHunk::AddFile {
                path: PathBuf::from(path),
                contents: String::new(),
            });
            self.state.mode = StreamingParserMode::AddFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(StreamHunk::DeleteFile {
                path: PathBuf::from(path),
            });
            self.state.mode = StreamingParserMode::DeleteFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.state.hunks.push(StreamHunk::UpdateFile {
                path: PathBuf::from(path),
                move_path: None,
                chunks: Vec::new(),
            });
            self.state.mode = StreamingParserMode::UpdateFile {
                hunk_line_number: self.line_number,
            };
            return Ok(true);
        }
        Ok(false)
    }

    fn process_line(&mut self, line: &str) -> Result<(), ParseError> {
        let trimmed = line.trim();
        match self.state.mode.clone() {
            StreamingParserMode::NotStarted => {
                if trimmed == BEGIN_PATCH_MARKER {
                    self.state.mode = StreamingParserMode::StartedPatch;
                    return Ok(());
                }
                Err(ParseError::InvalidFormat(
                    "The first line of the patch must be '*** Begin Patch'".to_string(),
                ))
            }
            StreamingParserMode::StartedPatch => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                Err(ParseError::InvalidHunk {
                    message: invalid_header_message(trimmed),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::AddFile => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                if let Some(line_to_add) = line.strip_prefix('+') {
                    if let Some(StreamHunk::AddFile { contents, .. }) = self.state.hunks.last_mut()
                    {
                        contents.push_str(line_to_add);
                        contents.push('\n');
                        return Ok(());
                    }
                }
                Err(ParseError::InvalidHunk {
                    message: invalid_header_message(trimmed),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::DeleteFile => {
                if self.handle_hunk_headers_and_end_patch(trimmed)? {
                    return Ok(());
                }
                Err(ParseError::InvalidHunk {
                    message: invalid_header_message(trimmed),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::UpdateFile { hunk_line_number } => {
                let update_line = line.trim_end();
                if self.handle_hunk_headers_and_end_patch(update_line)? {
                    return Ok(());
                }

                let Some(StreamHunk::UpdateFile {
                    move_path, chunks, ..
                }) = self.state.hunks.last_mut()
                else {
                    return Ok(());
                };

                if chunks.is_empty() && move_path.is_none() {
                    if let Some(move_to_path) = update_line.strip_prefix(MOVE_TO_MARKER) {
                        *move_path = Some(PathBuf::from(move_to_path));
                        self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                        return Ok(());
                    }
                }

                if (update_line == EMPTY_CHANGE_CONTEXT_MARKER
                    || update_line.starts_with(CHANGE_CONTEXT_MARKER))
                    && chunks
                        .last()
                        .is_some_and(|c| c.old_lines.is_empty() && c.new_lines.is_empty())
                {
                    return Err(ParseError::InvalidHunk {
                        message: format!(
                            "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                        ),
                        line_number: self.line_number,
                    });
                }

                if update_line == EMPTY_CHANGE_CONTEXT_MARKER {
                    chunks.push(StreamUpdateChunk {
                        change_context: None,
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        is_end_of_file: false,
                    });
                    self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                    return Ok(());
                }

                if let Some(change_context) = update_line.strip_prefix(CHANGE_CONTEXT_MARKER) {
                    chunks.push(StreamUpdateChunk {
                        change_context: Some(change_context.to_string()),
                        old_lines: Vec::new(),
                        new_lines: Vec::new(),
                        is_end_of_file: false,
                    });
                    self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                    return Ok(());
                }

                if update_line == EOF_MARKER {
                    if chunks
                        .last()
                        .is_some_and(|c| c.old_lines.is_empty() && c.new_lines.is_empty())
                    {
                        return Err(ParseError::InvalidHunk {
                            message: "Update hunk does not contain any lines".to_string(),
                            line_number: self.line_number,
                        });
                    }
                    if let Some(c) = chunks.last_mut() {
                        c.is_end_of_file = true;
                    }
                    self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                    return Ok(());
                }

                if line.is_empty() {
                    if chunks.is_empty() {
                        chunks.push(empty_chunk());
                    }
                    if let Some(c) = chunks.last_mut() {
                        c.old_lines.push(String::new());
                        c.new_lines.push(String::new());
                    }
                    self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                    return Ok(());
                }

                if let Some(line_to_add) = line.strip_prefix(' ') {
                    if chunks.is_empty() {
                        chunks.push(empty_chunk());
                    }
                    if let Some(c) = chunks.last_mut() {
                        c.old_lines.push(line_to_add.to_string());
                        c.new_lines.push(line_to_add.to_string());
                    }
                    self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                    return Ok(());
                }

                if let Some(line_to_add) = line.strip_prefix('+') {
                    if chunks.is_empty() {
                        chunks.push(empty_chunk());
                    }
                    if let Some(c) = chunks.last_mut() {
                        c.new_lines.push(line_to_add.to_string());
                    }
                    self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                    return Ok(());
                }

                if let Some(line_to_remove) = line.strip_prefix('-') {
                    if chunks.is_empty() {
                        chunks.push(empty_chunk());
                    }
                    if let Some(c) = chunks.last_mut() {
                        c.old_lines.push(line_to_remove.to_string());
                    }
                    self.state.mode = StreamingParserMode::UpdateFile { hunk_line_number };
                    return Ok(());
                }

                if chunks
                    .last()
                    .is_some_and(|c| !c.old_lines.is_empty() || !c.new_lines.is_empty())
                {
                    return Err(ParseError::InvalidHunk {
                        message: format!(
                            "Expected update hunk to start with a @@ context marker, got: '{line}'"
                        ),
                        line_number: self.line_number,
                    });
                }
                Err(ParseError::InvalidHunk {
                    message: format!(
                        "Unexpected line found in update hunk: '{line}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                    ),
                    line_number: self.line_number,
                })
            }
            StreamingParserMode::EndedPatch => Ok(()),
        }
    }
}

fn empty_chunk() -> StreamUpdateChunk {
    StreamUpdateChunk {
        change_context: None,
        old_lines: Vec::new(),
        new_lines: Vec::new(),
        is_end_of_file: false,
    }
}

fn invalid_header_message(trimmed: &str) -> String {
    format!(
        "'{trimmed}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(path: &str, contents: &str) -> StreamHunk {
        StreamHunk::AddFile {
            path: PathBuf::from(path),
            contents: contents.to_string(),
        }
    }

    fn del(path: &str) -> StreamHunk {
        StreamHunk::DeleteFile {
            path: PathBuf::from(path),
        }
    }

    fn upd(path: &str, move_path: Option<&str>, chunks: Vec<StreamUpdateChunk>) -> StreamHunk {
        StreamHunk::UpdateFile {
            path: PathBuf::from(path),
            move_path: move_path.map(PathBuf::from),
            chunks,
        }
    }

    fn chunk(
        ctx: Option<&str>,
        old: Vec<&str>,
        new: Vec<&str>,
        is_end_of_file: bool,
    ) -> StreamUpdateChunk {
        StreamUpdateChunk {
            change_context: ctx.map(str::to_string),
            old_lines: old.into_iter().map(str::to_string).collect(),
            new_lines: new.into_iter().map(str::to_string).collect(),
            is_end_of_file,
        }
    }

    #[test]
    fn streams_complete_lines_before_end_patch() {
        // A `+` line is only emitted once its trailing newline arrives — partial
        // line buffering must be byte-accurate (the LLM may chunk anywhere).
        let mut p = StreamingPatchParser::default();
        assert_eq!(
            p.push_delta("*** Begin Patch\n*** Add File: hi.txt\n+hello\n+wor")
                .unwrap(),
            vec![add("hi.txt", "hello\n")]
        );
        assert_eq!(
            p.push_delta("ld\n").unwrap(),
            vec![add("hi.txt", "hello\nworld\n")]
        );
    }

    #[test]
    fn update_with_move_to_and_one_chunk() {
        let mut p = StreamingPatchParser::default();
        assert_eq!(
            p.push_delta(
                "*** Begin Patch\n*** Update File: a.rs\n*** Move to: b.rs\n@@\n-old\n+new\n"
            )
            .unwrap(),
            vec![upd(
                "a.rs",
                Some("b.rs"),
                vec![chunk(None, vec!["old"], vec!["new"], false)]
            )]
        );
    }

    #[test]
    fn delete_emitted_on_terminating_newline() {
        let mut p = StreamingPatchParser::default();
        assert_eq!(
            p.push_delta("*** Begin Patch\n*** Delete File: gone.txt")
                .unwrap(),
            Vec::<StreamHunk>::new()
        );
        assert_eq!(p.push_delta("\n").unwrap(), vec![del("gone.txt")]);
    }

    #[test]
    fn large_patch_split_per_character_emits_monotonically() {
        // Stress test: feed the parser one char at a time and verify the visible
        // hunk count never decreases. Catches state-machine regressions where
        // partial input would throw away progress.
        let patch = "\
*** Begin Patch
*** Add File: a.md
+one
*** Update File: b.rs
@@
-old
+new
*** Delete File: c.rs
*** Update File: d.rs
*** Move to: e.rs
@@
-x
+y
*** End Patch";
        let mut p = StreamingPatchParser::default();
        let mut max_count = 0;
        let mut final_hunks = Vec::new();
        for ch in patch.chars() {
            let hunks = p.push_delta(&ch.to_string()).unwrap();
            assert!(
                hunks.len() >= max_count,
                "hunk count regressed: {} < {max_count}",
                hunks.len()
            );
            max_count = max_count.max(hunks.len());
            if !hunks.is_empty() {
                final_hunks = hunks;
            }
        }
        assert_eq!(final_hunks.len(), 4);
    }

    #[test]
    fn crlf_line_endings_are_normalized() {
        // CR is stripped before processing so Windows-style streams match LF
        // semantics. A bare CR inside content (without trailing LF) is left
        // attached — it's data, not a separator.
        let mut p = StreamingPatchParser::default();
        assert_eq!(
            p.push_delta(
                "*** Begin Patch\r\n*** Update File: f.txt\r\n@@\r\n-old\r\n+new\r\n*** End Patch\r\n"
            )
            .unwrap(),
            vec![upd("f.txt", None, vec![chunk(None, vec!["old"], vec!["new"], false)])]
        );

        let mut p = StreamingPatchParser::default();
        assert_eq!(
            p.push_delta(
                "*** Begin Patch\r\n*** Update File: f.txt\r\n@@\r\n-old\r\r\n+new\r\n*** End Patch\r\n"
            )
            .unwrap(),
            vec![upd("f.txt", None, vec![chunk(None, vec!["old\r"], vec!["new"], false)])]
        );
    }

    #[test]
    fn finish_processes_buffered_final_line_without_newline() {
        let mut p = StreamingPatchParser::default();
        let _ = p
            .push_delta("*** Begin Patch\n*** Add File: f.txt\n+hi\n*** End Patch")
            .unwrap();
        assert_eq!(p.finish().unwrap(), vec![add("f.txt", "hi\n")]);
    }

    #[test]
    fn finish_requires_end_patch_sentinel() {
        let mut p = StreamingPatchParser::default();
        let _ = p
            .push_delta("*** Begin Patch\n*** Add File: f.txt\n+hi\n")
            .unwrap();
        let err = p.finish().unwrap_err();
        assert!(matches!(err, ParseError::InvalidFormat(ref m) if m.contains("*** End Patch")));
    }

    #[test]
    fn rejects_non_begin_patch_first_line() {
        let mut p = StreamingPatchParser::default();
        let err = p.push_delta("garbage\n").unwrap_err();
        assert!(matches!(err, ParseError::InvalidFormat(ref m) if m.contains("*** Begin Patch")));
    }

    #[test]
    fn rejects_empty_update_hunk() {
        let mut p = StreamingPatchParser::default();
        let err = p
            .push_delta("*** Begin Patch\n*** Update File: f.txt\n*** End Patch\n")
            .unwrap_err();
        match err {
            ParseError::InvalidHunk { message, .. } => {
                assert!(message.contains("is empty"), "got: {message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_double_at_marker_without_body() {
        // After `@@\n` the next line must be a diff line; another `@@` means
        // the LLM emitted an empty chunk, which is always a bug.
        let mut p = StreamingPatchParser::default();
        let err = p
            .push_delta("*** Begin Patch\n*** Update File: f.txt\n@@\n@@\n")
            .unwrap_err();
        assert!(matches!(err, ParseError::InvalidHunk { .. }));
    }

    #[test]
    fn to_patch_operations_round_trips_to_batch_ast() {
        // The streaming parser's vector must convert losslessly to the same
        // PatchOperation AST the batch parser produces, so apply::apply_patch
        // doesn't need to know which path the patch came in on.
        let stream = vec![
            add("hi.txt", "hello\nworld\n"),
            del("gone.txt"),
            upd(
                "src.rs",
                Some("dst.rs"),
                vec![chunk(Some("fn main"), vec!["old"], vec!["new"], false)],
            ),
        ];
        let ops = to_patch_operations(&stream);
        assert_eq!(ops.len(), 3);
        match &ops[0] {
            PatchOperation::AddFile { path, content } => {
                assert_eq!(path, "hi.txt");
                // Trailing newline trimmed during conversion (matches batch
                // parser's joined form, which doesn't carry a trailing \n).
                assert_eq!(content, "hello\nworld");
            }
            other => panic!("expected AddFile, got {other:?}"),
        }
        match &ops[1] {
            PatchOperation::DeleteFile { path } => assert_eq!(path, "gone.txt"),
            other => panic!("expected DeleteFile, got {other:?}"),
        }
        match &ops[2] {
            PatchOperation::UpdateFile {
                path,
                move_to,
                hunks,
            } => {
                assert_eq!(path, "src.rs");
                assert_eq!(move_to.as_deref(), Some("dst.rs"));
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].search, "old");
                assert_eq!(hunks[0].replace, "new");
                assert_eq!(hunks[0].context_anchors, vec!["fn main".to_string()]);
            }
            other => panic!("expected UpdateFile, got {other:?}"),
        }
    }

    #[test]
    fn invalid_header_error_names_offender_line_and_valid_headers() {
        let mut p = StreamingPatchParser::default();
        let err = p
            .push_delta("*** Begin Patch\n*** Bogus: x\n")
            .unwrap_err();
        match err {
            ParseError::InvalidHunk {
                message,
                line_number,
            } => {
                assert_eq!(line_number, 2);
                assert!(
                    message.contains("'*** Bogus: x' is not a valid hunk header"),
                    "got: {message}"
                );
                assert!(message.contains("*** Add File: "), "got: {message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn finish_reports_empty_chunk_error_at_buffered_end_patch_line() {
        // `@@` opened a chunk that never received lines; the buffered final
        // `*** End Patch` (no trailing newline) must be counted as line 4 and
        // produce the empty-hunk message, not the unexpected-line one.
        let mut p = StreamingPatchParser::default();
        let _ = p
            .push_delta("*** Begin Patch\n*** Update File: f.txt\n@@\n*** End Patch")
            .unwrap();
        match p.finish().unwrap_err() {
            ParseError::InvalidHunk {
                message,
                line_number,
            } => {
                assert_eq!(line_number, 4);
                assert!(
                    message.contains("does not contain any lines"),
                    "got: {message}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn finish_rejects_buffered_content_line_that_is_not_end_patch() {
        // A trailing non-sentinel line must go through normal processing and
        // then fail the End Patch requirement — not be mistaken for the
        // sentinel and silently accepted.
        let mut p = StreamingPatchParser::default();
        let _ = p
            .push_delta("*** Begin Patch\n*** Add File: f.txt\n+hi")
            .unwrap();
        let err = p.finish().unwrap_err();
        assert!(matches!(err, ParseError::InvalidFormat(ref m) if m.contains("*** End Patch")));
    }

    #[test]
    fn insertion_only_and_deletion_only_chunks_are_valid() {
        let mut p = StreamingPatchParser::default();
        assert_eq!(
            p.push_delta(
                "*** Begin Patch\n*** Update File: f.txt\n@@\n+one\n@@ ctx\n-two\n*** End Patch\n"
            )
            .unwrap(),
            vec![upd(
                "f.txt",
                None,
                vec![
                    chunk(None, vec![], vec!["one"], false),
                    chunk(Some("ctx"), vec!["two"], vec![], false),
                ]
            )]
        );
    }

    #[test]
    fn eof_marker_accepted_after_insertion_only_chunk() {
        let mut p = StreamingPatchParser::default();
        assert_eq!(
            p.push_delta(
                "*** Begin Patch\n*** Update File: f.txt\n@@\n+line\n*** End of File\n*** End Patch\n"
            )
            .unwrap(),
            vec![upd("f.txt", None, vec![chunk(None, vec![], vec!["line"], true)])]
        );
    }

    #[test]
    fn move_to_rejected_after_first_chunk() {
        // `Move to:` is only valid immediately after the Update header; once a
        // chunk exists it must be treated as an unexpected line.
        let mut p = StreamingPatchParser::default();
        let err = p
            .push_delta("*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Move to: b.rs\n")
            .unwrap_err();
        match err {
            ParseError::InvalidHunk { message, .. } => {
                assert!(
                    message.contains("@@ context marker"),
                    "got: {message}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn junk_line_after_diff_lines_asks_for_context_marker() {
        // Whether the open chunk has only removals or only additions, an
        // unprefixed line must produce the "expected @@" diagnostic, not the
        // generic unexpected-line one reserved for empty chunks.
        for body in ["-x", "+x"] {
            let mut p = StreamingPatchParser::default();
            let err = p
                .push_delta(&format!(
                    "*** Begin Patch\n*** Update File: f.txt\n@@\n{body}\njunk\n"
                ))
                .unwrap_err();
            match err {
                ParseError::InvalidHunk { message, .. } => {
                    assert!(
                        message.contains("Expected update hunk to start with a @@ context marker"),
                        "body {body}: got: {message}"
                    );
                }
                other => panic!("body {body}: unexpected error: {other:?}"),
            }
        }
    }

    #[test]
    fn streaming_then_apply_via_conversion_works_end_to_end() {
        // Integration guard: a stream that builds an AddFile hunk, when fed
        // through to_patch_operations and then apply::apply_patch via the
        // public Patch wrapper, lands the file on disk identically to the
        // batch path. Catches regressions in the conversion layer.
        let dir = tempfile::tempdir().unwrap();
        let mut p = StreamingPatchParser::default();
        let _ = p
            .push_delta("*** Begin Patch\n*** Add File: greet.txt\n+hello\n+world\n*** End Patch\n")
            .unwrap();
        let hunks = p.finish().unwrap();
        let ops = to_patch_operations(&hunks);
        let patch = crate::parser::Patch { operations: ops };
        crate::apply::apply_patch(&patch, dir.path()).unwrap();
        let written = std::fs::read_to_string(dir.path().join("greet.txt")).unwrap();
        assert_eq!(written, "hello\nworld");
    }
}
