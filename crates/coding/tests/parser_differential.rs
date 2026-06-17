//! Differential harness: the batch parser (`parse_patch_with_mode`) and the
//! streaming parser (`StreamingPatchParser` + `to_patch_operations`) are two
//! independent parse paths over the same Patch DSL. Divergence between them is
//! a latent bug for a critical invariant (a hunk that parses one way when
//! applied in batch and another when streamed). This corpus feeds identical
//! input through both and asserts they agree — same accept/reject decision,
//! and, when accepted, the same `Vec<PatchOperation>` after normalizing the
//! one incidental field the streaming parser doesn't track (`source_line`).
//!
//! Scope is the surface both parsers implement: Add / Update / Delete with
//! `@@` context, `*** End of File`, multi-hunk, multi-file, and CRLF. The
//! batch parser additionally accepts `*** Move File:` and unified-diff input,
//! which the streaming parser intentionally does not produce — those are out
//! of scope here and covered by the batch parser's own suite.

use coding::parser::{parse_patch_with_mode, ParseMode, PatchOperation};
use coding::streaming_parser::{to_patch_operations, StreamingPatchParser};

/// Batch parse → operations (Lenient mode: its boundary handling matches the
/// streaming parser, which keys off `*** Begin/End Patch` without the strict
/// surrounding-whitespace checks).
fn batch_ops(input: &str) -> Result<Vec<PatchOperation>, String> {
    parse_patch_with_mode(input, ParseMode::Lenient)
        .map(|p| p.operations)
        .map_err(|e| format!("{e:?}"))
}

/// Whole-input streaming parse → the same `PatchOperation` AST the batch path
/// produces, mirroring how a completed stream is consumed in production.
fn streaming_ops(input: &str) -> Result<Vec<PatchOperation>, String> {
    let mut parser = StreamingPatchParser::default();
    parser.push_delta(input).map_err(|e| format!("{e:?}"))?;
    let hunks = parser.finish().map_err(|e| format!("{e:?}"))?;
    Ok(to_patch_operations(&hunks))
}

/// Zero out `source_line`: the streaming parser doesn't track a per-hunk line
/// number (it always emits 0), so comparing it would flag a non-semantic
/// difference. Everything else — paths, search/replace text, move target,
/// end-of-file anchoring, operation order — is compared verbatim.
fn normalize(ops: &mut [PatchOperation]) {
    for op in ops.iter_mut() {
        if let PatchOperation::UpdateFile { hunks, .. } = op {
            for h in hunks.iter_mut() {
                h.source_line = 0;
            }
        }
    }
}

fn assert_agree(name: &str, input: &str) {
    match (batch_ops(input), streaming_ops(input)) {
        (Ok(mut b), Ok(mut s)) => {
            normalize(&mut b);
            normalize(&mut s);
            assert_eq!(
                b, s,
                "parsers accepted `{name}` but produced different ASTs"
            );
        }
        (Err(_), Err(_)) => {} // both reject → agreement
        (b, s) => {
            panic!("accept/reject mismatch on `{name}`:\n  batch     = {b:?}\n  streaming = {s:?}")
        }
    }
}

/// Accepted inputs across the shared DSL surface. Each must parse identically
/// (modulo `source_line`) through both engines.
#[test]
fn batch_and_streaming_agree_on_accepted_patches() {
    let corpus: &[(&str, &str)] = &[
        (
            "add_single_file",
            "*** Begin Patch\n*** Add File: hello.txt\n+hello\n+world\n*** End Patch\n",
        ),
        (
            "delete_single_file",
            "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n",
        ),
        (
            "update_one_hunk",
            "*** Begin Patch\n*** Update File: a.txt\n@@\n context\n-old\n+new\n*** End Patch\n",
        ),
        (
            "update_with_context_hint",
            "*** Begin Patch\n*** Update File: src/lib.rs\n@@ fn main\n context\n-let x = 1;\n+let x = 2;\n*** End Patch\n",
        ),
        (
            "update_multi_hunk",
            "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+TWO\n@@\n three\n-four\n+FOUR\n*** End Patch\n",
        ),
        (
            "update_end_of_file",
            "*** Begin Patch\n*** Update File: a.txt\n@@\n keep\n+appended\n*** End of File\n*** End Patch\n",
        ),
        (
            "multi_file_add_update_delete",
            "*** Begin Patch\n*** Add File: new.txt\n+fresh\n*** Update File: edit.txt\n@@\n a\n-b\n+B\n*** Delete File: old.txt\n*** End Patch\n",
        ),
        (
            "update_with_move_to",
            "*** Begin Patch\n*** Update File: from.txt\n*** Move to: to.txt\n@@\n a\n-b\n+B\n*** End Patch\n",
        ),
        (
            "add_empty_file",
            "*** Begin Patch\n*** Add File: empty.txt\n*** End Patch\n",
        ),
        (
            "crlf_line_endings",
            "*** Begin Patch\r\n*** Add File: crlf.txt\r\n+one\r\n+two\r\n*** End Patch\r\n",
        ),
        (
            "pure_insertion_hunk",
            "*** Begin Patch\n*** Update File: a.txt\n@@\n before\n+inserted\n after\n*** End Patch\n",
        ),
        (
            "pure_deletion_hunk",
            "*** Begin Patch\n*** Update File: a.txt\n@@\n before\n-removed\n after\n*** End Patch\n",
        ),
    ];
    for (name, input) in corpus {
        assert_agree(name, input);
    }
}

/// Malformed inputs both engines must reject. A divergence here (one accepts,
/// the other rejects) is exactly the latent bug this harness guards against.
#[test]
fn batch_and_streaming_agree_on_rejected_patches() {
    let corpus: &[(&str, &str)] = &[
        (
            "missing_end_patch",
            "*** Begin Patch\n*** Add File: a.txt\n+x\n",
        ),
        ("no_begin_patch", "*** Add File: a.txt\n+x\n*** End Patch\n"),
        ("empty_input", ""),
    ];
    for (name, input) in corpus {
        assert_agree(name, input);
    }
}
