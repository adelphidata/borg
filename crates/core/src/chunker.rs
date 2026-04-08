use crate::tokenizer::estimate_tokens;

/// A chunk of content with line position metadata.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub content: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// The kind of markdown structural block.
#[derive(Debug, Clone, PartialEq)]
enum BlockKind {
    Paragraph,
    Heading(u8),
    CodeFence,
}

/// A structural block of markdown content.
#[derive(Debug, Clone)]
struct Block {
    #[allow(dead_code)] // only read in tests
    kind: BlockKind,
    content: String,
    start_line: usize, // 1-indexed
    end_line: usize,   // 1-indexed, inclusive
}

/// Parse markdown text into structural blocks, respecting code fences and headings.
fn parse_markdown_blocks(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let mut blocks: Vec<Block> = Vec::new();
    let mut current_lines: Vec<&str> = Vec::new();
    let mut current_kind = BlockKind::Paragraph;
    let mut block_start = 1usize; // 1-indexed
    let mut in_fence = false;
    let mut fence_marker: Option<&str> = None;

    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1; // 1-indexed
        let trimmed = line.trim();

        // Check for code fence boundaries
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = &trimmed[..3];
            if in_fence {
                // Closing fence — must match opening marker
                if fence_marker == Some(marker) {
                    current_lines.push(line);
                    // Emit the code fence block
                    blocks.push(Block {
                        kind: BlockKind::CodeFence,
                        content: current_lines.join("\n"),
                        start_line: block_start,
                        end_line: line_num,
                    });
                    current_lines.clear();
                    in_fence = false;
                    fence_marker = None;
                    block_start = line_num + 1;
                    continue;
                }
                // Not matching marker — treat as content inside fence
                current_lines.push(line);
                continue;
            } else {
                // Opening fence — flush current block first
                if !current_lines.is_empty() {
                    let content = current_lines.join("\n");
                    if !content.trim().is_empty() {
                        blocks.push(Block {
                            kind: current_kind.clone(),
                            content,
                            start_line: block_start,
                            end_line: line_num - 1,
                        });
                    }
                    current_lines.clear();
                }
                in_fence = true;
                fence_marker = Some(if trimmed.starts_with("```") {
                    "```"
                } else {
                    "~~~"
                });
                current_lines.push(line);
                current_kind = BlockKind::CodeFence;
                block_start = line_num;
                continue;
            }
        }

        // Inside a code fence — accumulate everything
        if in_fence {
            current_lines.push(line);
            continue;
        }

        // Check for heading
        if let Some(level) = heading_level(trimmed) {
            // Flush current block
            if !current_lines.is_empty() {
                let content = current_lines.join("\n");
                if !content.trim().is_empty() {
                    blocks.push(Block {
                        kind: current_kind.clone(),
                        content,
                        start_line: block_start,
                        end_line: line_num - 1,
                    });
                }
                current_lines.clear();
            }
            current_kind = BlockKind::Heading(level);
            current_lines.push(line);
            block_start = line_num;
            continue;
        }

        // Blank line — flush current block as paragraph boundary
        if trimmed.is_empty() {
            if !current_lines.is_empty() {
                let content = current_lines.join("\n");
                if !content.trim().is_empty() {
                    blocks.push(Block {
                        kind: current_kind.clone(),
                        content,
                        start_line: block_start,
                        end_line: line_num - 1,
                    });
                }
                current_lines.clear();
                current_kind = BlockKind::Paragraph;
            }
            block_start = line_num + 1;
            continue;
        }

        // Regular content line
        if current_lines.is_empty() {
            block_start = line_num;
            if current_kind != BlockKind::Paragraph {
                current_kind = BlockKind::Paragraph;
            }
        }
        current_lines.push(line);
    }

    // Flush remaining (including unclosed code fences)
    if !current_lines.is_empty() {
        let content = current_lines.join("\n");
        if !content.trim().is_empty() {
            let kind = if in_fence {
                BlockKind::CodeFence
            } else {
                current_kind
            };
            blocks.push(Block {
                kind,
                content,
                start_line: block_start,
                end_line: lines.len(),
            });
        }
    }

    blocks
}

/// Detect heading level (1-6) from a trimmed line, or None.
fn heading_level(trimmed: &str) -> Option<u8> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes).is_some_and(|&b| b == b' ') {
        Some(hashes as u8)
    } else {
        None
    }
}

/// Split markdown content into chunks of approximately `chunk_size_tokens` tokens,
/// with `overlap_tokens` of trailing content from the previous chunk prepended.
///
/// Markdown-aware: respects code fences (never splits mid-fence), uses headings
/// as natural chunk boundaries.
pub fn chunk_content(text: &str, chunk_size_tokens: usize, overlap_tokens: usize) -> Vec<Chunk> {
    if text.is_empty() {
        return Vec::new();
    }

    let blocks = parse_markdown_blocks(text);
    if blocks.is_empty() {
        return Vec::new();
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current_content = String::new();
    let mut current_start = blocks[0].start_line;
    let mut current_end = blocks[0].end_line;
    let mut current_tokens = 0usize;

    for block in &blocks {
        let block_tokens = estimate_tokens(&block.content);

        if current_tokens > 0 && current_tokens + block_tokens > chunk_size_tokens {
            // Emit current chunk
            chunks.push(Chunk {
                content: current_content.clone(),
                start_line: current_start,
                end_line: current_end,
            });

            // Start new chunk with overlap
            if overlap_tokens > 0 {
                let overlap_content = extract_trailing_tokens(&current_content, overlap_tokens);
                current_content = overlap_content;
                if !current_content.is_empty() {
                    current_content.push_str("\n\n");
                }
                current_tokens = estimate_tokens(&current_content);
            } else {
                current_content.clear();
                current_tokens = 0;
            }
            current_start = block.start_line;
        }

        if !current_content.is_empty() && current_tokens > 0 {
            current_content.push_str("\n\n");
            current_tokens += 1; // ~1 token for separator
        }
        current_content.push_str(&block.content);
        // Incremental tracking: avoids O(n²) re-estimation of the full accumulated string.
        // BPE tokenizers are not perfectly additive, so this may drift slightly from the true
        // count. The error is bounded and acceptable for chunk sizing.
        current_tokens += block_tokens;
        current_end = block.end_line;
    }

    // Emit final chunk
    if !current_content.is_empty() {
        chunks.push(Chunk {
            content: current_content,
            start_line: current_start,
            end_line: current_end,
        });
    }

    chunks
}

/// Extract trailing content from text that is approximately `target_tokens` tokens.
fn extract_trailing_tokens(text: &str, target_tokens: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result_lines: Vec<&str> = Vec::new();
    let mut tokens = 0;

    for line in lines.iter().rev() {
        let line_tokens = estimate_tokens(line);
        if tokens + line_tokens > target_tokens && !result_lines.is_empty() {
            break;
        }
        result_lines.push(line);
        tokens += line_tokens;
    }

    result_lines.reverse();
    result_lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_empty_content() {
        let chunks = chunk_content("", 400, 80);
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_short_content_single_chunk() {
        let text = "Hello world.\n\nThis is a short memory.";
        let chunks = chunk_content(text, 400, 80);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
    }

    #[test]
    fn chunk_respects_size_limit() {
        let paragraph = "A ".repeat(100); // ~100 tokens
        let text = (0..10)
            .map(|_| paragraph.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_content(&text, 400, 80);
        assert!(chunks.len() > 1, "should split into multiple chunks");
        for chunk in &chunks {
            let tokens = estimate_tokens(&chunk.content);
            assert!(
                tokens <= 500,
                "chunk should not greatly exceed limit: {tokens}"
            );
        }
    }

    #[test]
    fn chunk_overlap_shares_content() {
        let paragraph = "Word ".repeat(120); // ~120 tokens per paragraph
        let text = (0..5)
            .map(|_| paragraph.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_content(&text, 200, 50);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn chunk_line_numbers_are_monotonic() {
        let paragraph = "Word ".repeat(60); // ~60 tokens each
        let text = (0..8)
            .map(|_| paragraph.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_content(&text, 200, 0);
        assert!(chunks.len() >= 2, "should have multiple chunks");
        for i in 1..chunks.len() {
            assert!(
                chunks[i].start_line >= chunks[i - 1].start_line,
                "start_line should be monotonically increasing"
            );
        }
    }

    #[test]
    fn chunk_zero_overlap() {
        let paragraph = "Word ".repeat(120);
        let text = (0..5)
            .map(|_| paragraph.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_content(&text, 200, 0);
        assert!(chunks.len() >= 2);
    }

    // -- markdown-aware tests --

    #[test]
    fn chunk_preserves_code_fence() {
        // Code fence block should never be split
        let code = "x = 1\n".repeat(50); // many lines inside fence
        let text = format!(
            "# Header\n\nSome text.\n\n```python\n{}```\n\nMore text.",
            code
        );
        let chunks = chunk_content(&text, 100, 0);
        // Verify no chunk contains an opening ``` without a closing one
        for chunk in &chunks {
            let opens = chunk.content.matches("```python").count();
            let closes = chunk.content.matches("```").count().saturating_sub(opens);
            if opens > 0 {
                assert!(closes >= opens, "code fence split across chunks");
            }
        }
    }

    #[test]
    fn chunk_splits_on_headings() {
        let section_a = format!("## Section A\n\n{}", "Word ".repeat(80));
        let section_b = format!("## Section B\n\n{}", "Word ".repeat(80));
        let text = format!("{section_a}\n\n{section_b}");
        let chunks = chunk_content(&text, 100, 0);
        assert!(chunks.len() >= 2, "should split on heading boundaries");
    }

    #[test]
    fn chunk_heading_accumulation() {
        // Small headings under budget should be accumulated together
        let text = "## A\n\nShort.\n\n## B\n\nAlso short.";
        let chunks = chunk_content(&text, 400, 0);
        assert_eq!(chunks.len(), 1, "small sections should be in one chunk");
    }

    #[test]
    fn chunk_markdown_backward_compat() {
        // No headings/fences — should behave like paragraph splitting
        let text = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.";
        let chunks = chunk_content(&text, 400, 0);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.contains("First paragraph"));
        assert!(chunks[0].content.contains("Third paragraph"));
    }

    #[test]
    fn chunk_unclosed_code_fence() {
        // Unclosed fence should be treated as a single block
        let text = "Some text.\n\n```python\nx = 1\ny = 2";
        let chunks = chunk_content(&text, 400, 0);
        assert!(!chunks.is_empty());
        // The fence content should be in some chunk
        let all_content: String = chunks.iter().map(|c| c.content.clone()).collect();
        assert!(all_content.contains("```python"));
        assert!(all_content.contains("x = 1"));
    }

    #[test]
    fn heading_level_detection() {
        assert_eq!(heading_level("# Title"), Some(1));
        assert_eq!(heading_level("## Sub"), Some(2));
        assert_eq!(heading_level("###### H6"), Some(6));
        assert_eq!(heading_level("#nospace"), None);
        assert_eq!(heading_level("####### H7"), None); // only 1-6
        assert_eq!(heading_level("not a heading"), None);
    }

    #[test]
    fn parse_blocks_basic() {
        let text = "# Title\n\nParagraph one.\n\n```rust\nlet x = 1;\n```\n\nParagraph two.";
        let blocks = parse_markdown_blocks(text);
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].kind, BlockKind::Heading(1));
        assert_eq!(blocks[1].kind, BlockKind::Paragraph);
        assert_eq!(blocks[2].kind, BlockKind::CodeFence);
        assert_eq!(blocks[3].kind, BlockKind::Paragraph);
    }

    #[test]
    fn parse_blocks_tilde_fence() {
        let text = "~~~\ncode\n~~~";
        let blocks = parse_markdown_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::CodeFence);
    }

    #[test]
    fn chunk_consecutive_code_fences() {
        let text = "```\nfirst block\n```\n\n```\nsecond block\n```";
        let chunks = chunk_content(text, 400, 0);
        assert!(!chunks.is_empty());
        let all_content: String = chunks
            .iter()
            .map(|c| c.content.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all_content.contains("first block"));
        assert!(all_content.contains("second block"));
    }

    #[test]
    fn chunk_four_backtick_fence() {
        let text = "````\ncode here\n````";
        let blocks = parse_markdown_blocks(text);
        // Four backticks start with "```" so they should be detected as a fence
        assert!(!blocks.is_empty());
    }

    #[test]
    fn chunk_overlap_exceeds_size() {
        // overlap > chunk_size should not panic or infinite loop
        let paragraph = "Word ".repeat(100);
        let text = (0..5)
            .map(|_| paragraph.clone())
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = chunk_content(&text, 50, 200);
        assert!(!chunks.is_empty(), "should produce at least one chunk");
    }

    #[test]
    fn chunk_empty_code_block() {
        let text = "Before.\n\n```\n```\n\nAfter.";
        let chunks = chunk_content(text, 400, 0);
        assert!(!chunks.is_empty());
        let all: String = chunks
            .iter()
            .map(|c| c.content.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("Before"));
        assert!(all.contains("After"));
    }

    #[test]
    fn chunk_heading_as_last_line() {
        let text = "Some content.\n\n## Final Heading";
        let chunks = chunk_content(text, 400, 0);
        assert!(!chunks.is_empty());
        let all: String = chunks
            .iter()
            .map(|c| c.content.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("Final Heading"));
    }

    #[test]
    fn chunk_heading_with_special_chars() {
        let text = "## Section @#$!\n\nContent here.";
        let blocks = parse_markdown_blocks(text);
        assert!(!blocks.is_empty());
        assert_eq!(blocks[0].kind, BlockKind::Heading(2));
    }
}
