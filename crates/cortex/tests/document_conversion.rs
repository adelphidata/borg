//! End-to-end coverage for the document → Markdown read path.
//!
//! Every case drives the real `read_file` handler against a real file in its
//! format (fixtures in `tests/fixtures/documents`, regenerable with the
//! `generate.py` beside them). Word, PowerPoint, Excel, OpenDocument, RTF,
//! EPUB and PDF must come back as Markdown; text and CSV must come back
//! untouched, because converting them would break the read → `apply_patch`
//! round trip that editing them depends on.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use cortex::config::Config;
use cortex::tool_handlers::handle_read_file;
use cortex::types::ToolOutput;
use serde_json::json;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/documents")
        .join(name)
}

/// Read a path through the real handler and return its text output.
fn read(path: &str) -> String {
    read_with(json!({ "path": path }))
}

fn read_with(args: serde_json::Value) -> String {
    match handle_read_file(&args, &Config::default()).expect("read_file handler failed") {
        ToolOutput::Text(text) => text,
        other => panic!("expected text output, got {other:?}"),
    }
}

/// The whole point of the feature: a binary office document the agent could
/// previously not read at all comes back as Markdown carrying its content.
/// One case per container family — they share a serializer, so the risk being
/// covered is per-format *parsing*, not rendering.
#[test]
fn every_document_format_converts_to_markdown() {
    // (fixture, format label in the header, phrases only that container's
    // parser can recover). Spreadsheets carry cells rather than prose, so
    // they are checked on their own content, not a shared title.
    let cases: [(&str, &str, &[&str]); 9] = [
        (
            "sample.docx",
            "Word (.docx)",
            &["Quarterly Report", "Widgets shipped: 4200 units"],
        ),
        (
            "sample.pptx",
            "PowerPoint (.pptx)",
            &["Quarterly Report", "Widgets shipped: 4200 units"],
        ),
        ("sample.xlsx", "Excel", &["Widgets", "Bristol", "4200"]),
        (
            "sample.odt",
            "OpenDocument Text (.odt)",
            &["Quarterly Report", "Widgets shipped: 4200 units"],
        ),
        (
            "sample.ods",
            "OpenDocument Spreadsheet (.ods)",
            &["Widgets", "Bristol", "4200"],
        ),
        (
            "sample.odp",
            "OpenDocument Presentation (.odp)",
            &["Quarterly Report", "Widgets shipped: 4200 units"],
        ),
        (
            "sample.rtf",
            "RTF",
            &["Quarterly Report", "Widgets shipped: 4200 units"],
        ),
        (
            "sample.epub",
            "EPUB",
            &["Quarterly Report", "Widgets shipped: 4200 units"],
        ),
        (
            "sample.pdf",
            "PDF",
            &["Quarterly Report", "Widgets shipped: 4200 units"],
        ),
    ];

    for (name, label, phrases) in cases {
        let out = read(fixture(name).to_str().unwrap());
        for phrase in phrases {
            assert!(
                out.contains(phrase),
                "{name}: `{phrase}` missing from conversion:\n{out}"
            );
        }
        assert!(
            out.contains(label),
            "{name}: header should name the source format as `{label}`:\n{out}"
        );
        assert!(
            !out.contains("may be binary"),
            "{name}: must not fall through to the binary-file error:\n{out}"
        );
    }
}

/// Structure is the reason to convert rather than strip text: headings and
/// tables have to survive into the Markdown, or the model gets a wall of
/// prose it cannot navigate.
#[test]
fn conversion_preserves_document_structure() {
    let doc = read(fixture("sample.docx").to_str().unwrap());
    assert!(
        doc.contains("# Quarterly Report"),
        "docx heading should render as a Markdown heading:\n{doc}"
    );
    assert!(
        doc.contains("- Expand the Bristol line"),
        "docx bullet should render as a Markdown list item:\n{doc}"
    );

    let sheet = read(fixture("sample.xlsx").to_str().unwrap());
    assert!(
        sheet.contains('|'),
        "spreadsheet rows should render as a Markdown table:\n{sheet}"
    );
    assert!(
        sheet.contains("Widgets") && sheet.contains("Bristol"),
        "spreadsheet cells missing:\n{sheet}"
    );

    let deck = read(fixture("sample.pptx").to_str().unwrap());
    assert!(
        deck.contains("Outlook"),
        "second slide missing — every slide must be converted, not just the first:\n{deck}"
    );
}

/// A converted view cannot be written back through `apply_patch`: the file on
/// disk is binary. The header is the only thing telling the model that, so it
/// has to be present on every converted read.
#[test]
fn converted_documents_are_labeled_as_uneditable() {
    for name in ["sample.docx", "sample.xlsx", "sample.pdf", "sample.epub"] {
        let out = read(fixture(name).to_str().unwrap());
        assert!(
            out.starts_with('['),
            "{name}: conversion header must lead the output:\n{out}"
        );
        assert!(
            out.contains("apply_patch"),
            "{name}: header must warn that the file is not editable as text:\n{out}"
        );
    }
}

/// CSV is in anydoc's format list, but converting it to a Markdown table would
/// mean the agent could no longer edit a CSV it had just read. It must keep
/// coming back byte-for-byte.
#[test]
fn csv_is_still_read_as_raw_text() {
    let out = read(fixture("sample.csv").to_str().unwrap());
    assert_eq!(
        out,
        "Part,Region,Units\nWidgets,Bristol,4200\nSprockets,Bristol,875\n"
    );
    assert!(
        !out.contains('|'),
        "CSV must not be rendered as a Markdown table: {out}"
    );
    assert!(
        !out.contains("apply_patch"),
        "CSV is editable text and must carry no conversion header: {out}"
    );
}

/// Reading ordinary source and text files is the overwhelmingly common case;
/// adding document support must not have touched it.
#[test]
fn text_files_are_unchanged_by_the_document_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.rs");
    std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();

    let out = read(path.to_str().unwrap());
    assert_eq!(out, "fn main() {\n    println!(\"hi\");\n}\n");
}

/// Format is detected from the content signature, so a document that was
/// saved or downloaded under the wrong name still converts instead of failing
/// as unreadable binary — and the mismatch is disclosed rather than hidden.
///
/// The fixture is deliberately large: a ZIP package is identified through its
/// central directory, which lives at the *end* of the container, so detecting
/// from a fixed leading prefix would silently work on toy files and fail on
/// every real document.
#[test]
fn a_document_with_a_lying_extension_still_converts() {
    let path = fixture("mislabeled.txt");
    assert!(
        std::fs::metadata(&path).unwrap().len() > 16 * 1024,
        "fixture must stay too large for prefix-only detection to pass by luck"
    );

    let out = read_with(json!({ "path": path.to_str().unwrap(), "max_chars": 2_000_000 }));
    assert!(
        out.contains("Quarterly Report"),
        "content should be recovered despite the .txt extension:\n{out}"
    );
    assert!(
        out.contains("despite the file extension"),
        "the extension mismatch must be disclosed:\n{out}"
    );
    assert!(
        out.contains("Word (.docx)"),
        "header should name what the bytes actually are:\n{out}"
    );
}

/// Conversion happens after the security check, never before: a document
/// inside a blocked path must not be parsed and must not leak its content.
#[test]
fn blocked_paths_are_enforced_before_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let secret_dir = dir.path().join(".ssh");
    std::fs::create_dir(&secret_dir).unwrap();
    let path = secret_dir.join("report.docx");
    std::fs::copy(fixture("sample.docx"), &path).unwrap();

    let out = read(path.to_str().unwrap());
    assert!(
        out.contains("denied"),
        "expected an access denial, got:\n{out}"
    );
    assert!(
        !out.contains("Quarterly Report"),
        "blocked document content leaked into the output:\n{out}"
    );
}

/// Windowing applies to the Markdown, not the bytes — a large deck or report
/// has to be readable in pieces the same way a large source file is.
#[test]
fn offset_and_limit_window_the_converted_markdown() {
    let path = fixture("sample.docx");
    let full = read(path.to_str().unwrap());
    let windowed = read_with(json!({ "path": path.to_str().unwrap(), "offset": 1, "limit": 2 }));

    assert!(
        windowed.len() < full.len(),
        "limit should shorten the output:\n{windowed}"
    );
    assert!(
        windowed.contains("showing lines"),
        "a partial read must say so:\n{windowed}"
    );
    assert!(
        windowed.contains("apply_patch"),
        "the header must survive windowing:\n{windowed}"
    );
    assert!(
        !windowed.contains("Retire the legacy SKU"),
        "content past the limit should be cut:\n{windowed}"
    );
}

#[test]
fn max_chars_truncates_converted_output() {
    let path = fixture("sample.docx");
    let out = read_with(json!({ "path": path.to_str().unwrap(), "max_chars": 40 }));
    assert!(
        out.contains("truncated"),
        "expected truncation notice:\n{out}"
    );
    assert!(
        !out.contains("Retire the legacy SKU"),
        "truncated output should not reach the end of the document:\n{out}"
    );
}

/// A corrupt or truncated document must produce a diagnosable message, not a
/// panic and not a silent empty read.
#[test]
fn a_corrupt_document_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.docx");
    // A real ZIP local-file header followed by garbage: recognizable as a
    // container, unusable as a document.
    std::fs::write(
        &path,
        b"PK\x03\x04\x14\x00\x00\x00\x08\x00garbage-not-a-document",
    )
    .unwrap();

    let out = read(path.to_str().unwrap());
    assert!(
        out.contains("Error reading document") || out.contains("No extractable text"),
        "expected a conversion error, got:\n{out}"
    );
}

/// The size guard exists to bound memory, so it must refuse the file rather
/// than attempt the parse.
#[test]
fn oversized_documents_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.docx");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(cortex::constants::MAX_DOCUMENT_FILE_SIZE as u64 + 1)
        .unwrap();
    drop(file);

    let out = read(path.to_str().unwrap());
    assert!(
        out.contains("too large"),
        "expected a size refusal, got:\n{out}"
    );
}

/// PDFs were the one format that already worked, through `pdf-extract`. The
/// new engine has to keep clearing that bar — same text, better structure.
#[test]
fn pdf_reading_did_not_regress() {
    let out = read(fixture("sample.pdf").to_str().unwrap());
    assert!(out.contains("Quarterly Report"), "PDF text missing:\n{out}");
    assert!(out.contains("4200"), "PDF body text missing:\n{out}");
    assert!(
        !out.contains("fallback"),
        "a text-layer PDF should convert natively, not through the fallback:\n{out}"
    );
}

/// A part-scanned PDF converts "successfully" while whole sections are
/// silently missing. The read must disclose how many pages yielded no text
/// and which ones, while still returning the text that was there.
#[test]
fn a_part_scanned_pdf_warns_about_its_empty_pages() {
    let out = read(fixture("mixed_pages.pdf").to_str().unwrap());
    assert!(
        out.contains("1 of 3 pages have no extractable text"),
        "the coverage gap must be counted:\n{out}"
    );
    assert!(
        out.contains("(page 2)"),
        "the empty page must be named:\n{out}"
    );
    assert!(
        out.contains("Quarterly Report") && out.contains("Bristol"),
        "the text pages must still come through:\n{out}"
    );
}

/// The warning is for partial coverage only: a PDF whose every page has text
/// must read exactly as before, with no coverage chatter.
#[test]
fn a_fully_texted_pdf_gets_no_coverage_warning() {
    let out = read(fixture("sample.pdf").to_str().unwrap());
    assert!(
        !out.contains("no extractable text"),
        "full coverage must not warn:\n{out}"
    );
}

/// A PDF with no text layer has nothing to extract by any engine. It must say
/// so plainly — an empty success would read as "this document is blank".
#[test]
fn a_pdf_without_a_text_layer_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scanned.pdf");
    std::fs::write(
        &path,
        b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n",
    )
    .unwrap();

    let out = read(path.to_str().unwrap());
    assert!(
        out.contains("No extractable text") || out.contains("Error reading document"),
        "expected an explicit empty-extraction message, got:\n{out}"
    );
    assert!(
        !out.contains("apply_patch"),
        "nothing was converted, so no conversion header should appear:\n{out}"
    );
}

/// Directories, missing files and images keep their existing behavior — the
/// document branch sits between them and must not have captured any.
#[test]
fn non_document_paths_keep_their_existing_behavior() {
    let missing = read("/nonexistent/report.docx");
    assert!(missing.contains("not found"), "got: {missing}");

    let dir = tempfile::tempdir().unwrap();
    let as_dir = read(dir.path().to_str().unwrap());
    assert!(as_dir.contains("directory"), "got: {as_dir}");
}
