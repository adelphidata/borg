//! Chunker integration tests.
//!
//! Tests content chunking with various document types, verifying
//! markdown-aware splitting, code fence preservation, and token budgets.

#![allow(
    clippy::approx_constant,
    clippy::assertions_on_constants,
    clippy::const_is_empty,
    clippy::expect_used,
    clippy::field_reassign_with_default,
    clippy::identity_op,
    clippy::items_after_test_module,
    clippy::len_zero,
    clippy::manual_range_contains,
    clippy::needless_borrow,
    clippy::needless_collect,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::unwrap_used,
    clippy::useless_format,
    clippy::useless_vec
)]

use borg_core::chunker::chunk_content;

// ── Test: empty content returns no chunks ──

#[test]
fn empty_content_no_chunks() {
    let chunks = chunk_content("", 100, 0);
    assert!(chunks.is_empty());
}

// ── Test: small content fits in single chunk ──

#[test]
fn small_content_single_chunk() {
    let chunks = chunk_content("Hello world", 1000, 0);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].content, "Hello world");
    assert_eq!(chunks[0].start_line, 1);
    assert_eq!(chunks[0].end_line, 1);
}

// ── Test: large content split into multiple chunks ──

#[test]
fn large_content_multiple_chunks() {
    // Generate content that is definitely larger than chunk_size.
    // Each heading creates a new "block" in the chunker, so use headings
    // to ensure block boundaries exist for splitting.
    let mut sections = Vec::new();
    for i in 1..=20 {
        sections.push(format!("# Section {i}\n"));
        for j in 1..=20 {
            sections.push(format!(
                "Paragraph {j} of section {i} with enough words to consume several BPE tokens in the tokenizer.\n"
            ));
        }
    }
    let text = sections.join("\n");
    // Use 100 tokens per chunk — the ~400 lines should produce many chunks
    let chunks = chunk_content(&text, 100, 0);
    assert!(
        chunks.len() > 1,
        "Should produce multiple chunks, got {}",
        chunks.len()
    );
}

// ── Test: code fences never split ──

#[test]
fn code_fences_preserved() {
    let text = "Some intro text.\n\n```python\ndef hello():\n    print('hi')\n    return True\n```\n\nSome outro text.";
    let chunks = chunk_content(text, 20, 0);

    // No chunk should start in the middle of a code fence
    for chunk in &chunks {
        let lines: Vec<&str> = chunk.content.lines().collect();
        let fence_opens = lines.iter().filter(|l| l.starts_with("```")).count();
        // If a chunk has a fence open, it should also have the close
        // (fences should be 0 or even)
        if fence_opens > 0 {
            assert!(
                fence_opens % 2 == 0 || chunk.content.starts_with("```"),
                "Code fence should not be split mid-block"
            );
        }
    }
}

// ── Test: heading boundaries respected ──

#[test]
fn heading_boundaries_respected() {
    let text = "# Section One\n\nContent for section one.\n\n# Section Two\n\nContent for section two.\n\n# Section Three\n\nContent for section three.";
    let chunks = chunk_content(text, 30, 0);

    if chunks.len() > 1 {
        // Chunks should tend to start at headings
        let starts_with_heading = chunks
            .iter()
            .skip(1) // first chunk starts at beginning
            .filter(|c| c.content.starts_with('#'))
            .count();
        assert!(
            starts_with_heading > 0,
            "Some chunk boundaries should align with headings"
        );
    }
}

// ── Test: overlap produces shared content ──

#[test]
fn overlap_produces_shared_content() {
    let lines: Vec<String> = (1..=50).map(|i| format!("Line {i}")).collect();
    let text = lines.join("\n");
    let chunks = chunk_content(&text, 20, 5);

    if chunks.len() >= 2 {
        // With overlap, chunks should share some content
        let c1_lines: Vec<&str> = chunks[0].content.lines().collect();
        let c2_lines: Vec<&str> = chunks[1].content.lines().collect();

        // The end of chunk 1 should overlap with the start of chunk 2
        let c1_end = &c1_lines[c1_lines.len().saturating_sub(3)..];
        let c2_start = &c2_lines[..3.min(c2_lines.len())];

        let has_overlap = c1_end.iter().any(|line| c2_start.contains(line));
        assert!(has_overlap, "Overlapping chunks should share content");
    }
}

// ── Test: line numbers are monotonically increasing ──

#[test]
fn line_numbers_monotonic() {
    let lines: Vec<String> = (1..=80).map(|i| format!("Content line {i}")).collect();
    let text = lines.join("\n");
    let chunks = chunk_content(&text, 30, 0);

    for (i, chunk) in chunks.iter().enumerate() {
        assert!(
            chunk.start_line <= chunk.end_line,
            "Chunk {i}: start_line ({}) should <= end_line ({})",
            chunk.start_line,
            chunk.end_line
        );
        if i > 0 {
            assert!(
                chunk.start_line >= chunks[i - 1].start_line,
                "Chunk {i}: start_line should be >= previous chunk's start_line"
            );
        }
    }
}

// ── Test: zero overlap works ──

#[test]
fn zero_overlap_no_duplicates() {
    let lines: Vec<String> = (1..=30).map(|i| format!("Line {i}")).collect();
    let text = lines.join("\n");
    let chunks = chunk_content(&text, 15, 0);

    if chunks.len() >= 2 {
        // Without overlap, consecutive chunks should not share the same start line
        for i in 1..chunks.len() {
            assert!(
                chunks[i].start_line > chunks[i - 1].start_line,
                "Chunks should advance without overlap"
            );
        }
    }
}

// ── Test: multi-fence document ──

#[test]
fn multi_fence_document() {
    let text = r#"# README

## Installation

```bash
npm install my-package
```

## Usage

```javascript
const pkg = require('my-package');
pkg.init();
```

## API

```typescript
interface Config {
    debug: boolean;
    port: number;
}
```
"#;
    let chunks = chunk_content(text, 30, 0);

    // All content should be covered
    let total_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
    assert!(total_content.contains("npm install"));
    assert!(total_content.contains("pkg.init()"));
    assert!(total_content.contains("debug: boolean"));
}

// ── Test: tilde fences handled ──

#[test]
fn tilde_fences_handled() {
    let text = "Intro.\n\n~~~\nfenced with tildes\n~~~\n\nOutro.";
    let chunks = chunk_content(text, 20, 0);
    let total: String = chunks.iter().map(|c| c.content.as_str()).collect();
    assert!(total.contains("fenced with tildes"));
}

// ── Test: single very long line ──

#[test]
fn single_very_long_line() {
    let text = "word ".repeat(500);
    let chunks = chunk_content(&text, 50, 0);
    assert!(!chunks.is_empty(), "Should produce at least one chunk");
}
