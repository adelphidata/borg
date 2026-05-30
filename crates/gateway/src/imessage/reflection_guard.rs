/// Detect assistant metadata markers in inbound text that indicate
/// the message is a reflected/leaked internal prompt rather than a
/// genuine user message.
///
/// Returns the name of the detected marker, or None if clean.
pub fn detect_reflected_content(text: &str) -> Option<&'static str> {
    // Mask code blocks so a user legitimately quoting `<thinking>` in a
    // fenced snippet doesn't trip the reflection heuristic.
    let regions = non_code_regions(text);

    for region in &regions {
        if region.contains("#####") {
            return Some("internal separator");
        }
        if region.contains("assistant to =") || region.contains("assistant to=") {
            return Some("role marker");
        }
        if region.contains("<thinking>") || region.contains("</thinking>") {
            return Some("thinking tag");
        }
        if region.contains("<thought>") || region.contains("</thought>") {
            return Some("thought tag");
        }
        if region.contains("<relevant_memories>") || region.contains("</relevant_memories>") {
            return Some("memory tag");
        }
        if region.contains("<internal>") || region.contains("</internal>") {
            return Some("internal tag");
        }
    }

    None
}

/// Extract text regions that are NOT inside code blocks (` or ```).
fn non_code_regions(text: &str) -> Vec<&str> {
    let mut regions = Vec::new();
    let mut remaining = text;
    let mut inside_code = false;

    while !remaining.is_empty() {
        if inside_code {
            // Find closing fence
            if let Some(pos) = remaining.find("```") {
                remaining = &remaining[pos + 3..];
                inside_code = false;
            } else {
                break; // rest is inside code block
            }
        } else if let Some(pos) = remaining.find("```") {
            // Text before the fence is a non-code region
            if pos > 0 {
                regions.push(&remaining[..pos]);
            }
            remaining = &remaining[pos + 3..];
            inside_code = true;
        } else {
            // No more fences — rest is non-code
            regions.push(remaining);
            break;
        }
    }

    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_passes() {
        assert!(detect_reflected_content("Hey, how are you?").is_none());
        assert!(detect_reflected_content("Can you help with my code?").is_none());
    }

    #[test]
    fn detects_thinking_tags() {
        assert_eq!(
            detect_reflected_content("Here is <thinking>some thought</thinking>"),
            Some("thinking tag")
        );
    }

    #[test]
    fn detects_internal_tags() {
        assert_eq!(
            detect_reflected_content("text <internal>hidden</internal> more"),
            Some("internal tag")
        );
    }

    #[test]
    fn detects_separators() {
        assert_eq!(
            detect_reflected_content("some text\n#####\nmore text"),
            Some("internal separator")
        );
    }

    #[test]
    fn ignores_markers_inside_code_blocks() {
        let text = "Look at this:\n```\n<thinking>example</thinking>\n```\nPretty cool right?";
        assert!(detect_reflected_content(text).is_none());
    }

    #[test]
    fn detects_markers_outside_code_blocks() {
        let text = "<thinking>leaked</thinking>\n```\ncode here\n```";
        assert_eq!(detect_reflected_content(text), Some("thinking tag"));
    }
}
