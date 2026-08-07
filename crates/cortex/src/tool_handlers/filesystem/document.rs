//! Binary-document → Markdown conversion for the file-read path.
//!
//! Word, PowerPoint, Excel, OpenDocument, RTF, EPUB and PDF are opaque bytes
//! to `read_to_string`, so before this module the agent could only see PDFs
//! (via `pdf-extract`) and got "the file may be binary" for everything else.
//! [`anydoc`] parses all of them locally — pure Rust, no services, no models —
//! and renders one Markdown serialization, so a 2003 `.doc` and a modern
//! `.pptx` come out shaped the same.
//!
//! Two deliberate limits on what gets converted:
//!
//! - **Text formats stay raw.** CSV is in anydoc's format list but is *not*
//!   converted here: it is already readable, and rendering it as a Markdown
//!   table would break the read→`apply_patch` round trip that editing a CSV
//!   depends on. The same reasoning keeps every other plain-text format out.
//! - **Conversion is lossy and one-way.** The bytes on disk cannot be edited
//!   through the Markdown view, so every converted read is labeled with
//!   [`conversion_header`] to keep the model from patching a binary file.

use std::path::Path;

use anydoc::ConvertError;
pub use anydoc::Format;

use crate::constants;

/// Extensions that are converted to Markdown on read.
///
/// Deliberately a subset of what [`anydoc`] handles: every entry names a
/// format whose bytes are opaque to a text read. CSV is excluded on purpose —
/// see the module docs.
const CONVERTED_EXTENSIONS: &[&str] = &[
    // Word
    "doc", "docx", "docm", // PowerPoint
    "ppt", "pps", "pot", "pptx", "pptm", "ppsx", "ppsm", // Excel
    "xls", "xlsx", "xlsm", "xlsb", // OpenDocument
    "odt", "ods", "odp", // Everything else opaque
    "rtf", "epub", "pdf",
];

/// A document that was converted to Markdown, and how.
#[derive(Debug)]
pub struct ConvertedDocument {
    /// GitHub-Flavored Markdown rendering of the document.
    pub markdown: String,
    /// The format the bytes were actually parsed as, which is not necessarily
    /// what the extension claimed.
    pub format: Format,
    /// True when `anydoc` could not convert a PDF and `pdf-extract` produced
    /// the text instead. Fallback output is plain text, not Markdown.
    pub used_pdf_fallback: bool,
}

/// True when a file with this extension should be converted rather than read
/// as text. The extension is passed without a leading dot and is matched
/// case-insensitively.
pub fn is_convertible_extension(ext: &str) -> bool {
    let ext = ext.to_ascii_lowercase();
    CONVERTED_EXTENSIONS.contains(&ext.as_str())
}

/// Convert a document file to Markdown.
///
/// The format is taken from the content signature first and the extension
/// only as a fallback, so a mislabeled file still converts as what it is.
pub fn convert(path: &Path) -> Result<ConvertedDocument, DocumentError> {
    let metadata = std::fs::metadata(path).map_err(DocumentError::Read)?;
    if metadata.len() > constants::MAX_DOCUMENT_FILE_SIZE as u64 {
        return Err(DocumentError::TooLarge {
            bytes: metadata.len(),
            limit: constants::MAX_DOCUMENT_FILE_SIZE,
        });
    }
    let bytes = std::fs::read(path).map_err(DocumentError::Read)?;
    convert_bytes(&bytes, Format::from_path(path))
}

/// Convert in-memory document bytes to Markdown. `extension_hint` supplies the
/// format for inputs whose content carries no signature.
pub fn convert_bytes(
    bytes: &[u8],
    extension_hint: Option<Format>,
) -> Result<ConvertedDocument, DocumentError> {
    let Some(format) = Format::from_bytes(bytes).or(extension_hint) else {
        return Err(DocumentError::Unrecognized);
    };

    // Container formats are ZIP archives, so the on-disk size bounds the
    // *compressed* bytes only. A small crafted archive expands into arbitrarily
    // large XML and then into an equally large Markdown string, all resident
    // before anything downstream gets to window it — the on-disk ceiling alone
    // is no defense. anydoc does not bound decompression, so hold a much lower
    // ceiling for these and refuse the expansion outright past it.
    if is_container_format(format) && bytes.len() > constants::MAX_CONTAINER_DOCUMENT_FILE_SIZE {
        return Err(DocumentError::TooLarge {
            bytes: bytes.len() as u64,
            limit: constants::MAX_CONTAINER_DOCUMENT_FILE_SIZE,
        });
    }

    let converted = without_panicking("anydoc", || anydoc::to_markdown_bytes(bytes, format))
        .unwrap_or_else(|| {
            Err(ConvertError::Unsupported(
                "parser panicked on malformed input".into(),
            ))
        });

    match converted {
        Ok(markdown) if markdown.len() > constants::MAX_DOCUMENT_MARKDOWN_BYTES => {
            // The expansion got past the input ceiling anyway. Refuse rather
            // than hand a multi-gigabyte string to the windowing code, which
            // would keep it alive for the length of the call.
            Err(DocumentError::Expanded {
                bytes: markdown.len() as u64,
            })
        }
        Ok(markdown) => Ok(ConvertedDocument {
            markdown,
            format,
            used_pdf_fallback: false,
        }),
        // `anydoc` rejects PDFs it cannot extract structured text from —
        // notably scanned/image-only ones. `pdf-extract` was the previous
        // engine on this path, so it stays as a second attempt rather than
        // regressing PDFs that used to read.
        Err(err) if format == Format::Pdf => match pdf_fallback(bytes) {
            Some(text) => Ok(ConvertedDocument {
                markdown: text,
                format,
                used_pdf_fallback: true,
            }),
            None => Err(DocumentError::Convert(err)),
        },
        Err(err) => Err(DocumentError::Convert(err)),
    }
}

/// Run a parser over untrusted bytes, downgrading a panic to `None`.
///
/// Document parsers panic on malformed structure rather than returning an
/// error — `pdf-extract` does it on a Type3 font whose `Widths` array is
/// short, which real PDFs in the wild have. These bytes arrive from anywhere
/// the agent can read, including channel attachments, so a malformed document
/// must never be able to take the agent down.
fn without_panicking<T>(
    engine: &'static str,
    parse: impl FnOnce() -> T + std::panic::UnwindSafe,
) -> Option<T> {
    match std::panic::catch_unwind(parse) {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::warn!(engine, "document parser panicked on malformed input");
            None
        }
    }
}

/// Extract PDF text with the legacy engine. Returns `None` when it also fails
/// or yields nothing worth showing, so the caller reports the original error
/// rather than an empty success.
fn pdf_fallback(bytes: &[u8]) -> Option<String> {
    let text = match without_panicking("pdf-extract", || pdf_extract::extract_text_from_mem(bytes))?
    {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(error = %e, "pdf fallback extraction failed");
            return None;
        }
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

/// ZIP-container formats, whose on-disk size says nothing about how much they
/// expand to. OLE (`.doc`/`.ppt`/`.xls`) and the flat formats are not archives,
/// so they stay on the plain file-size ceiling.
fn is_container_format(format: Format) -> bool {
    matches!(
        format,
        Format::Docx | Format::Pptx | Format::Excel | Format::Odt | Format::Ods | Format::Odp
    ) || format == Format::Epub
}

/// Why a document could not be converted.
#[derive(Debug)]
pub enum DocumentError {
    /// The file could not be read off disk.
    Read(std::io::Error),
    /// The file is past the size ceiling for its format.
    TooLarge {
        /// Actual file size in bytes.
        bytes: u64,
        /// The ceiling that was exceeded.
        limit: usize,
    },
    /// The document parsed, but expanded past
    /// [`constants::MAX_DOCUMENT_MARKDOWN_BYTES`] of Markdown.
    Expanded {
        /// Size of the produced Markdown in bytes.
        bytes: u64,
    },
    /// Neither the content signature nor the extension named a known format.
    Unrecognized,
    /// The parser rejected the document.
    Convert(ConvertError),
}

impl std::fmt::Display for DocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocumentError::Read(e) => write!(f, "could not read file: {e}"),
            DocumentError::TooLarge { bytes, limit } => write!(
                f,
                "document too large ({} MB). Max {} MB.",
                bytes / (1024 * 1024),
                limit / (1024 * 1024)
            ),
            DocumentError::Expanded { bytes } => write!(
                f,
                "document expanded to {} MB of text, past the {} MB ceiling.",
                bytes / (1024 * 1024),
                constants::MAX_DOCUMENT_MARKDOWN_BYTES / (1024 * 1024)
            ),
            DocumentError::Unrecognized => {
                write!(f, "unrecognized document format")
            }
            DocumentError::Convert(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DocumentError {}

/// Human-readable name for a format, used in the conversion header.
///
/// Matched exhaustively on purpose: anydoc is pre-1.0, and a new format
/// arriving should fail this build so it gets a label and a place in
/// [`CONVERTED_EXTENSIONS`] rather than silently reading as "document".
pub fn format_label(format: Format) -> &'static str {
    match format {
        Format::Doc => "Word (.doc)",
        Format::Docx => "Word (.docx)",
        Format::Odt => "OpenDocument Text (.odt)",
        Format::Pdf => "PDF",
        Format::Ppt => "PowerPoint (.ppt)",
        Format::Pptx => "PowerPoint (.pptx)",
        Format::Rtf => "RTF",
        Format::Epub => "EPUB",
        Format::Excel => "Excel",
        Format::Ods => "OpenDocument Spreadsheet (.ods)",
        Format::Odp => "OpenDocument Presentation (.odp)",
        Format::Csv => "CSV",
    }
}

/// The one-line notice that precedes converted content.
///
/// The model must be able to tell a converted view from the file itself: the
/// bytes on disk are binary, so an `apply_patch` against this text would be
/// nonsense. Naming the source format also tells it how much structure to
/// trust (a spreadsheet's table, a deck's slide headings).
pub fn conversion_header(doc: &ConvertedDocument, extension_mismatch: bool) -> String {
    let label = format_label(doc.format);
    let mut header = if extension_mismatch {
        format!("[Content is {label} despite the file extension — converted to Markdown")
    } else {
        format!("[{label} converted to Markdown")
    };
    if doc.used_pdf_fallback {
        header.push_str(" (fallback text extraction — structure not preserved)");
    }
    header.push_str("; the file on disk is binary and cannot be edited with apply_patch]\n\n");
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convertible_extensions_cover_every_opaque_family() {
        for ext in [
            "doc", "docx", "docm", "ppt", "pptx", "pps", "xls", "xlsx", "xlsb", "odt", "ods",
            "odp", "rtf", "epub", "pdf",
        ] {
            assert!(
                is_convertible_extension(ext),
                "`.{ext}` is opaque to a text read and must be converted"
            );
        }
    }

    #[test]
    fn convertible_extension_match_is_case_insensitive() {
        // Windows-authored attachments routinely arrive as `REPORT.DOCX`.
        assert!(is_convertible_extension("DOCX"));
        assert!(is_convertible_extension("Pdf"));
    }

    /// Converting CSV would render it as a Markdown table, and the agent could
    /// no longer round-trip an edit back through `apply_patch`. Plain-text
    /// formats must keep reaching the raw text path.
    #[test]
    fn text_formats_are_never_converted() {
        for ext in [
            "csv", "txt", "md", "rs", "json", "tsv", "log", "html", "xml",
        ] {
            assert!(
                !is_convertible_extension(ext),
                "`.{ext}` is editable text and must be read raw"
            );
        }
    }

    /// Content detection is what rescues a mislabeled document, so it must
    /// never claim signature-less plain text — otherwise ordinary files would
    /// start taking the conversion path.
    #[test]
    fn plain_text_is_never_detected_as_a_document() {
        for text in [
            &b"name,qty\nbolt,4\n"[..],
            b"# A markdown heading\n",
            b"fn main() {}\n",
            b"",
        ] {
            let err = convert_bytes(text, None).unwrap_err();
            assert!(
                matches!(err, DocumentError::Unrecognized),
                "plain text was claimed as a document: {err:?}"
            );
        }
    }

    #[test]
    fn too_large_documents_are_refused_before_parsing() {
        // The guard bounds peak memory, so it must trip on metadata alone —
        // never after the file has already been read in.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.docx");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(constants::MAX_DOCUMENT_FILE_SIZE as u64 + 1)
            .unwrap();
        drop(file);

        let err = convert(&path).unwrap_err();
        assert!(matches!(err, DocumentError::TooLarge { .. }), "got {err:?}");
        assert!(err.to_string().contains("too large"), "got {err}");
    }

    /// A `.docx`/`.xlsx`/`.epub` is a ZIP archive, so the on-disk size bounds
    /// only the compressed bytes — a few MB of crafted archive expands into
    /// gigabytes of XML that the parser holds all at once. The plain 100 MB
    /// file ceiling is no defense against that, so container formats must trip
    /// their own much lower ceiling.
    #[test]
    fn container_formats_are_held_to_a_lower_ceiling_than_flat_ones() {
        let oversized = vec![0u8; constants::MAX_CONTAINER_DOCUMENT_FILE_SIZE + 1];

        // Well under MAX_DOCUMENT_FILE_SIZE, so only the container ceiling can
        // reject this.
        assert!(oversized.len() < constants::MAX_DOCUMENT_FILE_SIZE);

        let err = convert_bytes(&oversized, Some(Format::Docx)).unwrap_err();
        assert!(
            matches!(err, DocumentError::TooLarge { limit, .. }
                if limit == constants::MAX_CONTAINER_DOCUMENT_FILE_SIZE),
            "a container past its ceiling must be refused, got {err:?}"
        );

        // RTF is not an archive: it expands roughly linearly, so the same byte
        // count must get past the size guard and be judged on its content.
        let err = convert_bytes(&oversized, Some(Format::Rtf)).unwrap_err();
        assert!(
            !matches!(err, DocumentError::TooLarge { .. }),
            "a flat format must not inherit the container ceiling, got {err:?}"
        );
    }

    /// `pdf-extract` panics on this fixture's malformed Type3 font metadata.
    /// Reading a file must never take the agent down, so the fallback has to
    /// come back empty-handed instead of unwinding out of the tool call.
    #[test]
    fn a_panicking_pdf_parser_is_contained() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/documents/type3_font.pdf");
        let bytes = std::fs::read(&fixture).expect("type3_font.pdf fixture");

        // Guard the premise: if pdf-extract stops panicking on this input, the
        // test is no longer covering anything and should be revisited.
        let panicked =
            std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes)).is_err();
        assert!(
            panicked,
            "fixture no longer reproduces the pdf-extract panic this guard exists for"
        );

        assert!(pdf_fallback(&bytes).is_none());
    }

    #[test]
    fn missing_file_reports_a_read_error() {
        let err = convert(Path::new("/nonexistent/report.docx")).unwrap_err();
        assert!(matches!(err, DocumentError::Read(_)), "got {err:?}");
    }

    #[test]
    fn header_warns_against_patching_and_names_the_format() {
        let doc = ConvertedDocument {
            markdown: String::new(),
            format: Format::Docx,
            used_pdf_fallback: false,
        };
        let header = conversion_header(&doc, false);
        assert!(header.contains("Word (.docx)"), "got {header}");
        assert!(
            header.contains("apply_patch"),
            "the model must be told the view is not editable: {header}"
        );
        assert!(!header.contains("despite"), "no mismatch claimed: {header}");
    }

    #[test]
    fn header_flags_an_extension_mismatch() {
        let doc = ConvertedDocument {
            markdown: String::new(),
            format: Format::Pdf,
            used_pdf_fallback: false,
        };
        let header = conversion_header(&doc, true);
        assert!(
            header.contains("despite the file extension"),
            "got {header}"
        );
    }

    /// Fallback output is plain extracted text, not Markdown. Silently
    /// presenting it as a structured conversion would have the model trust
    /// headings and tables that are not there.
    #[test]
    fn header_discloses_the_pdf_fallback() {
        let doc = ConvertedDocument {
            markdown: String::new(),
            format: Format::Pdf,
            used_pdf_fallback: true,
        };
        let header = conversion_header(&doc, false);
        assert!(header.contains("fallback"), "got {header}");
        assert!(header.contains("structure not preserved"), "got {header}");
    }
}
