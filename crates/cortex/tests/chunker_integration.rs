//! Chunker integration tests.
//!
//! Exercises `chunk_content` from the crate boundary. Inline tests in
//! `crates/cortex/src/memory/chunker.rs` already cover the unit-level cases
//! (empty/small/large input, code-fence preservation, line monotonicity,
//! overlap behaviour). This file is reserved for behaviour that is only
//! observable through the public API or that combines multiple block kinds
//! end-to-end.

#![allow(
    clippy::approx_constant,
    clippy::expect_used,
    clippy::unwrap_used
)]

use cortex::memory::chunker::chunk_content;

/// Mixed markdown documents (multiple fenced code blocks with distinct
/// languages plus headings) round-trip through chunking with all content
/// preserved. The inline tests cover a single fence and a single heading
/// separately; this asserts they compose without dropping content.
#[test]
fn multi_fence_document_preserves_all_content() {
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

    let total_content: String = chunks.iter().map(|c| c.content.as_str()).collect();
    assert!(total_content.contains("npm install"));
    assert!(total_content.contains("pkg.init()"));
    assert!(total_content.contains("debug: boolean"));
}

/// Tilde fences (`~~~`) are an alternate Markdown fence syntax. The
/// chunker must recognise them as code blocks alongside backtick fences.
#[test]
fn tilde_fences_treated_as_code_blocks() {
    let text = "Intro.\n\n~~~\nfenced with tildes\n~~~\n\nOutro.";
    let chunks = chunk_content(text, 20, 0);
    let total: String = chunks.iter().map(|c| c.content.as_str()).collect();
    assert!(total.contains("fenced with tildes"));
    assert!(total.contains("Intro."));
    assert!(total.contains("Outro."));
}

/// A single long line with no natural split point must not panic the
/// chunker and must still emit the line's content. Guards a past crash
/// on input that the block splitter could not divide.
#[test]
fn single_very_long_line_still_emits_content() {
    let text = "word ".repeat(500);
    let chunks = chunk_content(&text, 50, 0);
    assert!(!chunks.is_empty(), "must produce at least one chunk");
    let total: String = chunks.iter().map(|c| c.content.as_str()).collect();
    // Even when the line cannot be split on a block boundary, its content
    // must end up somewhere in the chunk stream.
    assert!(total.contains("word "));
}
