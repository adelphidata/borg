//! Naming what a non-text file actually is.
//!
//! "The file may be binary" names a dead end without naming a way out: the
//! model has to guess whether some other tool in its toolset can open the
//! thing, and in a filesystem-only toolset that guess costs turns it never
//! recovers. A magic-byte sniff answers the real question — *what is this* —
//! in the same read that refused it.
//!
//! The extension is a claim; only the content earns a type name. That is why
//! this sniffs bytes rather than trusting the suffix.

/// Ordered magic-byte prefixes, first match wins. Longer prefixes sharing a
/// first byte come first so the specific signature is not shadowed by the
/// looser one.
const MAGIC_SIGNATURES: &[(&[u8], &str)] = &[
    (b"\x89PNG\r\n\x1a\n", "PNG image data"),
    (b"\xff\xd8\xff", "JPEG image data"),
    (b"GIF87a", "GIF image data"),
    (b"GIF89a", "GIF image data"),
    (b"RIFF", "RIFF container (WAV/AVI/WebP family)"),
    (b"%PDF-", "PDF document"),
    (b"PK\x03\x04", "ZIP archive (also docx/xlsx/jar/apk)"),
    (b"PK\x05\x06", "ZIP archive (empty)"),
    (b"\x1f\x8b", "gzip compressed data"),
    (b"BZh", "bzip2 compressed data"),
    (b"\xfd7zXZ\x00", "xz compressed data"),
    (b"7z\xbc\xaf\x27\x1c", "7-Zip archive"),
    (b"\x7fELF", "ELF executable"),
    (b"\xcf\xfa\xed\xfe", "Mach-O executable (64-bit)"),
    (b"\xce\xfa\xed\xfe", "Mach-O executable (32-bit)"),
    (b"\xca\xfe\xba\xbe", "Mach-O universal binary / Java class"),
    (b"SQLite format 3\x00", "SQLite database"),
    (b"OggS", "Ogg container"),
    (b"fLaC", "FLAC audio"),
    (b"ID3", "MP3 audio (ID3 tag)"),
    (b"\x00asm", "WebAssembly module"),
    (b"BM", "BMP image data"),
    (b"II*\x00", "TIFF image data (little-endian)"),
    (b"MM\x00*", "TIFF image data (big-endian)"),
    (b"MZ", "Windows PE executable"),
];

/// Best-effort human name for binary content, from its leading bytes.
///
/// Returns `"unknown binary"` rather than guessing when nothing matches — a
/// wrong type name is worse than none, because the model will act on it.
pub fn identify_binary_bytes(sample: &[u8]) -> &'static str {
    if sample.is_empty() {
        return "unknown binary";
    }
    // The ISO media family (MP4/MOV) leads with a big-endian box size, so the
    // first four bytes carry no signature at all. `ftyp` at offset 4 is the
    // part that identifies it.
    if sample.len() >= 8 && &sample[4..8] == b"ftyp" {
        return "ISO media container (MP4/MOV family)";
    }
    for (prefix, name) in MAGIC_SIGNATURES {
        if sample.starts_with(prefix) {
            return name;
        }
    }
    "unknown binary"
}

/// Render a byte count the way a person reads one.
fn human_size(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

/// One-line answer for a refused binary read: what it is, how big it is, and
/// that text is not the shape it comes in.
pub fn describe_binary_file(path: &std::path::Path) -> String {
    let sample = read_head(path);
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    format!(
        "Binary file ({}, {}) — cannot display as text.",
        identify_binary_bytes(&sample),
        human_size(size)
    )
}

/// Read the leading bytes only. Every signature above fits well inside this,
/// and a refused read must not pull a gigabyte into memory to say so.
fn read_head(path: &std::path::Path) -> Vec<u8> {
    use std::io::Read as _;
    const HEAD_BYTES: u64 = 32;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    // `read_to_end` over a bounded reader rather than one `read`: a single
    // `read` may return short, and the longest signature here needs 16 bytes
    // ("SQLite format 3\0"), so a short read would silently downgrade a known
    // type to "unknown binary".
    let mut buf = Vec::with_capacity(HEAD_BYTES as usize);
    match file.take(HEAD_BYTES).read_to_end(&mut buf) {
        Ok(_) => buf,
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the sniff: a file whose *name* says text but whose bytes
    /// say PNG must be named for its bytes. This is the case the extension
    /// guard cannot reach.
    #[test]
    fn content_names_the_type_even_when_the_extension_lies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();

        let described = describe_binary_file(&path);
        assert!(
            described.contains("PNG image data"),
            "must name the sniffed type, got: {described}"
        );
        assert!(
            described.contains("bytes"),
            "must disclose the size, got: {described}"
        );
    }

    /// Signatures that share leading bytes must not shadow one another, and a
    /// format identified by an interior field (ISO media's `ftyp`) must still
    /// resolve.
    #[test]
    fn overlapping_and_interior_signatures_resolve_to_the_specific_type() {
        let cases: &[(&[u8], &str)] = &[
            (b"PK\x03\x04rest", "ZIP archive (also docx/xlsx/jar/apk)"),
            (b"PK\x05\x06rest", "ZIP archive (empty)"),
            (b"GIF89a....", "GIF image data"),
            (
                b"\x00\x00\x00\x20ftypisom",
                "ISO media container (MP4/MOV family)",
            ),
            (b"SQLite format 3\x00", "SQLite database"),
            (b"MZ\x90\x00", "Windows PE executable"),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                identify_binary_bytes(bytes),
                *expected,
                "wrong type for {bytes:?}"
            );
        }
    }

    /// A leading size field with no `ftyp` behind it is not ISO media, and
    /// unrecognized bytes must not be given an invented name.
    #[test]
    fn unrecognized_bytes_are_not_given_an_invented_name() {
        assert_eq!(
            identify_binary_bytes(b"\x00\x00\x00\x20wxyzabcd"),
            "unknown binary"
        );
        assert_eq!(identify_binary_bytes(b""), "unknown binary");
        assert_eq!(identify_binary_bytes(b"\x13\x37\x42\x99"), "unknown binary");
    }

    /// Sizes are for a reader, so they scale units; the raw byte count is only
    /// useful below a kilobyte.
    #[test]
    fn sizes_scale_to_readable_units() {
        assert_eq!(human_size(512), "512 bytes");
        assert_eq!(human_size(4200), "4.1 KB");
        assert_eq!(human_size(3 * 1024 * 1024 + 512 * 1024), "3.5 MB");
    }
}
