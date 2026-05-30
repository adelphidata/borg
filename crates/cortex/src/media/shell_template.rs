//! Shell command template rendering for local media providers.

/// Render `{name}` placeholders for a command that will be executed via
/// `sh -c`.
///
/// Placeholder values are shell-escaped by default. If the operator already
/// placed a placeholder inside single or double quotes, only the quote-local
/// escaping is applied so existing templates like `cat '{audio_file}'` keep
/// working.
pub(crate) fn render(template: &str, placeholders: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < template.len() {
        let rest = &template[i..];
        if rest.starts_with("{{") {
            out.push('{');
            i += 2;
            continue;
        }
        if rest.starts_with("}}") {
            out.push('}');
            i += 2;
            continue;
        }
        if let Some(after_brace) = rest.strip_prefix('{') {
            if let Some(end_rel) = after_brace.find('}') {
                let name = &after_brace[..end_rel];
                if let Some((_, value)) = placeholders.iter().find(|(key, _)| *key == name) {
                    let quote = active_shell_quote(template, i);
                    out.push_str(&quote_for_context(value, quote));
                    i += end_rel + 2;
                    continue;
                }
            }
        }

        // `i < template.len()` guarantees a remaining char; advance one char.
        let Some(ch) = rest.chars().next() else { break };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn active_shell_quote(template: &str, position: usize) -> Option<char> {
    let mut quote = None;
    let mut escaped = false;
    for ch in template[..position].chars() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some('"') => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    quote = None;
                }
            }
            _ => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                }
            }
        }
    }
    quote
}

fn quote_for_context(value: &str, quote: Option<char>) -> String {
    match quote {
        Some('\'') => value.replace('\'', "'\\''"),
        Some('"') => escape_double_quoted(value),
        _ => shell_quote(value),
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn escape_double_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '"' | '$' | '`') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn renders_unquoted_placeholders_as_shell_quoted_values() {
        let rendered = render(
            "cat {audio_file} --lang {language}",
            &[("audio_file", "/tmp/a b.wav"), ("language", "en-US")],
        );
        assert_eq!(rendered, "cat '/tmp/a b.wav' --lang 'en-US'");
    }

    #[test]
    fn preserves_operator_single_quoted_templates() {
        let rendered = render(
            "cat '{audio_file}'",
            &[("audio_file", "/tmp/voice memo's final.wav")],
        );
        assert_eq!(rendered, "cat '/tmp/voice memo'\\''s final.wav'");
    }

    #[test]
    fn escapes_double_quoted_template_values() {
        let rendered = render(
            "say \"{text}\"",
            &[("text", "hello $(touch /tmp/borg-owned) \"friend\"")],
        );
        assert_eq!(
            rendered,
            "say \"hello \\$(touch /tmp/borg-owned) \\\"friend\\\"\""
        );
    }

    #[test]
    fn keeps_literal_braces_available() {
        let rendered = render(
            "printf '{{{format}}}' > {output_path}",
            &[("format", "wav"), ("output_path", "/tmp/out file")],
        );
        assert_eq!(rendered, "printf '{wav}' > '/tmp/out file'");
    }
}
