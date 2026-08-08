//! Incremental UTF-8 decoding for byte streams read in arbitrary chunks.
//!
//! A fixed-size pipe read can split a multibyte codepoint across two chunks;
//! decoding each chunk independently with `String::from_utf8_lossy` turns the
//! split character into two U+FFFD replacement chars. [`Utf8StreamDecoder`]
//! carries the trailing incomplete sequence (at most 3 bytes) from one chunk
//! to the next so the character decodes intact once its remaining bytes
//! arrive. Only *streaming* call sites need this — accumulate-then-decode-once
//! paths already see the whole byte run.

/// Stateful lossy UTF-8 decoder for one byte stream.
///
/// Feed each raw chunk to [`decode`](Self::decode); call
/// [`flush`](Self::flush) at end-of-stream so a dangling partial sequence is
/// surfaced (as a single replacement char) instead of silently dropped.
#[derive(Default)]
pub(crate) struct Utf8StreamDecoder {
    /// Held-back trailing bytes of an incomplete sequence (never more than 3).
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Lossily decode `chunk`, prepending any bytes held back from the
    /// previous call and holding back a trailing incomplete UTF-8 sequence
    /// for the next one. Interior invalid bytes are replaced with U+FFFD as
    /// `from_utf8_lossy` would.
    pub(crate) fn decode(&mut self, chunk: &[u8]) -> String {
        if chunk.is_empty() {
            // Nothing new: keep any pending bytes for a later chunk/flush.
            return String::new();
        }
        let owned;
        let data: &[u8] = if self.pending.is_empty() {
            chunk
        } else {
            let mut joined = std::mem::take(&mut self.pending);
            joined.extend_from_slice(chunk);
            owned = joined;
            &owned
        };
        let keep = incomplete_suffix_len(data);
        let (complete, rest) = data.split_at(data.len() - keep);
        self.pending = rest.to_vec();
        String::from_utf8_lossy(complete).into_owned()
    }

    /// End-of-stream: a dangling partial sequence can never complete, so it
    /// is rendered as one replacement char (matching what a whole-buffer
    /// lossy decode of the truncated stream would show).
    pub(crate) fn flush(&mut self) -> String {
        if self.pending.is_empty() {
            String::new()
        } else {
            self.pending.clear();
            '\u{FFFD}'.to_string()
        }
    }
}

/// Length of the trailing byte run that is the *start* of a multibyte UTF-8
/// sequence whose remaining bytes have not arrived yet (0 when the buffer
/// ends on a complete — or definitively invalid — boundary). At most 3: a
/// 4-byte sequence missing its last byte.
fn incomplete_suffix_len(data: &[u8]) -> usize {
    let len = data.len();
    // Only the last 3 bytes can be an incomplete suffix.
    let start = len.saturating_sub(3);
    for i in (start..len).rev() {
        let b = data[i];
        if b < 0x80 {
            // ASCII: everything after a complete char is continuation bytes
            // with no lead, i.e. invalid, not incomplete.
            return 0;
        }
        if b >= 0xC0 {
            // Lead byte: incomplete iff its declared length extends past the
            // end of the buffer. `needed == 0` marks an invalid lead
            // (0xC0/0xC1 handled by the lossy pass; >= 0xF8 impossible lead).
            let needed: usize = match b {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => 0,
            };
            let have = len - i;
            return if needed > have { have } else { 0 };
        }
        // Continuation byte (0x80..=0xBF): keep scanning backwards for its
        // lead within the 3-byte window.
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every possible split point of a multibyte-heavy string must decode
    /// back to the original with no replacement chars — the regression is a
    /// pipe read boundary landing mid-codepoint and producing two U+FFFD.
    #[test]
    fn split_multibyte_chars_decode_cleanly_at_every_boundary() {
        // 2-, 3-, and 4-byte codepoints plus ASCII interleaved.
        let fixtures = ["é ok", "héllo wörld", "中文输出", "a😀b😀c", "…✓ done 🚀"];
        for fixture in fixtures {
            let bytes = fixture.as_bytes();
            for split in 0..=bytes.len() {
                let mut dec = Utf8StreamDecoder::new();
                let mut out = dec.decode(&bytes[..split]);
                out.push_str(&dec.decode(&bytes[split..]));
                out.push_str(&dec.flush());
                assert_eq!(
                    out, fixture,
                    "split at byte {split} of {fixture:?} corrupted the stream"
                );
            }
        }
    }

    /// One-byte-at-a-time delivery is the worst case: a 4-byte emoji arrives
    /// over four reads and must still come out as one char.
    #[test]
    fn byte_at_a_time_stream_reassembles() {
        let fixture = "😀🇺🇸é";
        let mut dec = Utf8StreamDecoder::new();
        let mut out = String::new();
        for b in fixture.as_bytes() {
            out.push_str(&dec.decode(std::slice::from_ref(b)));
        }
        out.push_str(&dec.flush());
        assert_eq!(out, fixture);
    }

    /// A stream that ends mid-sequence must surface exactly one replacement
    /// char at flush — not silently drop the bytes.
    #[test]
    fn truncated_final_sequence_flushes_one_replacement_char() {
        let mut dec = Utf8StreamDecoder::new();
        // First 3 bytes of the 4-byte 😀.
        assert_eq!(dec.decode(&[0xF0, 0x9F, 0x98]), "");
        assert_eq!(dec.flush(), "\u{FFFD}");
        // Flush drains: a second flush emits nothing.
        assert_eq!(dec.flush(), "");
    }

    /// Genuinely invalid interior bytes must still be replaced — the decoder
    /// only defers *trailing incomplete* sequences, never bad bytes.
    #[test]
    fn interior_invalid_bytes_are_replaced_not_held() {
        let mut dec = Utf8StreamDecoder::new();
        let out = dec.decode(&[b'a', 0xFF, b'b']);
        assert_eq!(out, "a\u{FFFD}b");
        assert_eq!(dec.flush(), "");
    }

    /// A lone continuation-byte tail with no lead in reach is invalid, not
    /// incomplete: it must decode (as U+FFFD) now rather than being held
    /// forever waiting for bytes that can't complete it.
    #[test]
    fn continuation_bytes_without_a_lead_are_not_held_back() {
        let mut dec = Utf8StreamDecoder::new();
        let out = dec.decode(&[b'x', 0x80, 0x80]);
        assert!(out.starts_with('x'), "got {out:?}");
        assert!(
            out.contains('\u{FFFD}'),
            "invalid bytes must surface: {out:?}"
        );
        assert_eq!(dec.flush(), "");
    }
}
