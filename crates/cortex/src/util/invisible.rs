//! Detection of invisible and bidirectional-override Unicode scalars.
//!
//! A zero-width space inside `rm` or a right-to-left override that renders
//! `sh | base64 -d` backwards lets a command read as one thing and execute
//! as another. The shell policy denies such commands outright; the memory
//! injection scanner rejects the same families as hidden instructions.
//!
//! The set is deliberately narrower than "every format character". The
//! zero-width non-joiner and joiner (U+200C/U+200D) are mandatory spelling
//! in Persian, Urdu, Sinhala and Malayalam, the LTR/RTL marks (U+200E/
//! U+200F) are ordinary in mixed-direction prose, and the tag block spells
//! the subdivision flags. Denying those would refuse legitimate commit
//! messages and file contents with no way for the model to comply, so only
//! scalars with no plain-text use in a command are denied.

/// True for a scalar that renders as nothing or reorders what follows and
/// has no legitimate use inside a command: the zero-width space, the bidi
/// embeddings, overrides and isolates, the word joiner and invisible
/// operators, the byte-order mark and the Mongolian vowel separator.
pub fn is_invisible_or_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
            | '\u{180E}'
    )
}

fn is_tag_char(c: char) -> bool {
    matches!(c, '\u{E0000}'..='\u{E007F}')
}

/// True if `text` carries any invisible or bidi scalar that could hide the
/// real shape of a command. Tag characters (U+E0000..U+E007F) are allowed
/// only as the spelling of a subdivision flag, i.e. directly after the
/// black-flag base U+1F3F4 and up to the cancel tag U+E007F; anywhere else
/// they are as suspect as a zero-width space.
pub fn has_invisible_or_bidi(text: &str) -> bool {
    let mut in_flag_tags = false;
    let mut prev: Option<char> = None;
    for c in text.chars() {
        if is_tag_char(c) {
            if !in_flag_tags && prev != Some('\u{1F3F4}') {
                return true;
            }
            in_flag_tags = c != '\u{E007F}';
        } else {
            in_flag_tags = false;
            if is_invisible_or_bidi(c) {
                return true;
            }
        }
        prev = Some(c);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_hidden_scalars_and_legitimate_script_joiners() {
        let cases: &[(&str, bool)] = &[
            ("echo \u{202E}abc", true),
            ("rm\u{200B} -rf x", true),
            ("a\u{202A}b", true),
            ("a\u{2060}b", true),
            ("a\u{2064}b", true),
            ("a\u{2066}b", true),
            ("a\u{2069}b", true),
            ("\u{FEFF}ls", true),
            ("a\u{180E}b", true),
            // Tag characters outside a flag sequence hide text.
            ("ls\u{E0001}\u{E0020}", true),
            ("echo \u{E007F}", true),
            // A tag run after the cancel tag is a second, unanchored run.
            ("echo \u{1F3F4}\u{E0067}\u{E007F}\u{E0062}", true),
            // The Scotland flag is a legitimate tag sequence.
            (
                "echo \u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}",
                false,
            ),
            // Script-mandatory joiners and direction marks are plain text.
            (
                "git commit -m \"\u{645}\u{6cc}\u{200c}\u{634}\u{648}\u{62f}\"",
                false,
            ),
            ("rm\u{200D} -rf x", false),
            ("ls\u{200C}", false),
            ("a\u{200E}b", false),
            ("a\u{200F}b", false),
            (
                "git commit -m \"\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\"",
                false,
            ),
            ("echo caf\u{e9}", false),
            ("echo \u{65e5}\u{672c}\u{8a9e} \u{2014} ok", false),
            // Neighbouring code points outside the ranges stay clean.
            ("a\u{200A}b", false),
            ("a\u{2065}b", false),
            ("a\u{206A}b", false),
            ("", false),
        ];
        for (text, expected) in cases {
            assert_eq!(
                has_invisible_or_bidi(text),
                *expected,
                "{}",
                text.escape_unicode()
            );
        }
    }
}
