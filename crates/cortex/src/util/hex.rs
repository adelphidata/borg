//! Lowercase hex encode/decode helpers.
//!
//! The canonical decoder returns `anyhow::Result`; callers that need an
//! `Option` use `.ok()`.

use std::fmt::Write as _;

use anyhow::{bail, Result};

/// Hex-encode a byte slice to a lowercase hex string.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String is infallible.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Decode a lowercase or uppercase hex string into bytes.
pub fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("hex string must have even length");
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in bytes.chunks(2) {
        let hi = nibble(chunk[0])?;
        let lo = nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => bail!("invalid hex character: {c}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_lowercase_pairs() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0xab, 0xcd]), "abcd");
    }

    #[test]
    fn decode_roundtrips() {
        let bytes = vec![0x00, 0x12, 0xab, 0xff];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn decode_accepts_uppercase() {
        assert_eq!(hex_decode("ABCD").unwrap(), vec![0xab, 0xcd]);
    }

    #[test]
    fn decode_rejects_odd_length() {
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn decode_rejects_invalid_char() {
        assert!(hex_decode("zz").is_err());
    }
}
