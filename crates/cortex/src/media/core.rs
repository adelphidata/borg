use std::io::Cursor;

use anyhow::Result;
use base64::Engine;
use tracing::warn;

use crate::types::MediaData;

/// Max side length steps to try, descending.
const DIMENSION_STEPS: &[u32] = &[2048, 1536, 1280, 1024, 800];

/// JPEG quality levels to try, descending.
const QUALITY_STEPS: &[u8] = &[85, 75, 65, 55, 45, 35];

/// Pass the response through when its status is a success; otherwise bail
/// with `"<label> returned <status>: <body>"`, consuming the response so the
/// provider's error body lands in the message.
pub(crate) async fn ensure_success(
    resp: reqwest::Response,
    label: &str,
) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    anyhow::bail!("{label} returned {status}: {body}")
}

/// Read a response body into memory with a hard byte ceiling enforced
/// chunk-by-chunk while streaming.
///
/// `Content-Length` is checked first as a fast reject, but the running total
/// is the real boundary: the header can lie or be absent entirely (chunked
/// transfer encoding), and a post-hoc `bytes().await` length check only fires
/// after the whole body — however large — has been buffered into memory.
/// Aborting mid-stream drops the connection, so at most `max_bytes` (plus one
/// chunk) is ever resident.
///
/// `what` labels the payload in error messages ("image", "Slack file", …).
pub async fn read_body_capped(
    mut resp: reqwest::Response,
    max_bytes: u64,
    what: &str,
) -> Result<Vec<u8>> {
    use anyhow::Context;
    if let Some(reported) = resp.content_length() {
        if reported > max_bytes {
            anyhow::bail!("{what} too large ({reported} bytes, max {max_bytes})");
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("read {what} body"))?
    {
        if buf.len() as u64 + chunk.len() as u64 > max_bytes {
            anyhow::bail!(
                "{what} too large (body exceeded max {max_bytes} bytes mid-stream; \
                 Content-Length was absent or understated)"
            );
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Deserialize a JSON response body without echoing any of it on failure.
///
/// A serde error's `Display` quotes offending values (`invalid type: string
/// "…"`), and a remote body can hold tokens or anything else a server chose to
/// send. The error keeps only the category, position, and byte length — enough
/// to debug a bad payload, nothing of its content.
pub(crate) fn parse_json_body<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    what: &str,
) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| {
        anyhow::anyhow!(
            "{what}: invalid JSON ({:?} error at line {}, column {}; {} bytes)",
            e.classify(),
            e.line(),
            e.column(),
            bytes.len()
        )
    })
}

/// Parse a data URI like `data:image/png;base64,abc123` into a MediaData struct.
///
/// Validates the `;base64` marker is present and the data portion is non-empty.
pub fn parse_data_uri(uri: &str) -> Result<MediaData> {
    let rest = uri
        .strip_prefix("data:")
        .ok_or_else(|| anyhow::anyhow!("not a data URI"))?;
    let (header, data) = rest
        .split_once(",")
        .ok_or_else(|| anyhow::anyhow!("malformed data URI: missing comma"))?;
    if !header.contains(";base64") {
        anyhow::bail!("data URI missing ;base64 marker");
    }
    if data.is_empty() {
        anyhow::bail!("data URI has empty data");
    }
    let mime_type = header
        .split(';')
        .next()
        .unwrap_or("application/octet-stream")
        .to_string();
    Ok(MediaData {
        mime_type,
        data: data.to_string(),
        filename: None,
    })
}

/// Compress a base64-encoded image to fit within max_bytes.
/// Returns `(new_base64_data, new_mime_type)`. Passthrough if already small enough.
pub fn compress_image(
    base64_data: &str,
    mime_type: &str,
    max_bytes: usize,
) -> Result<(String, String)> {
    let engine = base64::engine::general_purpose::STANDARD;

    let validated =
        validate_base64(base64_data).ok_or_else(|| anyhow::anyhow!("invalid base64 data"))?;
    let raw_bytes = engine.decode(&validated)?;

    let (bytes, effective_mime) = if is_heic(mime_type) {
        match convert_heic_to_jpeg(&raw_bytes) {
            Ok(jpeg_bytes) => (jpeg_bytes, "image/jpeg".to_string()),
            Err(e) => {
                warn!("HEIC conversion failed: {e}");
                return Ok((base64_data.to_string(), mime_type.to_string()));
            }
        }
    } else {
        (raw_bytes, mime_type.to_string())
    };

    // If already small enough, return as-is. A converted HEIC is returned
    // as the JPEG it became — handing back the original container here
    // would undo the conversion.
    if bytes.len() <= max_bytes {
        if effective_mime != mime_type {
            return Ok((engine.encode(&bytes), effective_mime));
        }
        return Ok((base64_data.to_string(), mime_type.to_string()));
    }

    compress_decoded_image(&bytes, max_bytes)
}

/// [`compress_image`] for a caller that already holds the decoded bytes
/// (and has already validated them): no HEIC handling, no second base64
/// round-trip. Returns `(base64_data, mime_type)`; passthrough re-encodes
/// the bytes unchanged when they fit.
pub fn compress_image_bytes(
    bytes: &[u8],
    mime_type: &str,
    max_bytes: usize,
) -> Result<(String, String)> {
    if bytes.len() <= max_bytes {
        let engine = base64::engine::general_purpose::STANDARD;
        return Ok((engine.encode(bytes), mime_type.to_string()));
    }
    compress_decoded_image(bytes, max_bytes)
}

/// Re-encode `bytes` as a JPEG that fits in `max_bytes`, shrinking
/// dimensions and quality until it does.
fn compress_decoded_image(bytes: &[u8], max_bytes: usize) -> Result<(String, String)> {
    let engine = base64::engine::general_purpose::STANDARD;

    let img = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| anyhow::anyhow!("failed to guess image format: {e}"))?
        .decode()
        .map_err(|e| anyhow::anyhow!("failed to decode image: {e}"))?;

    // Grid search: dimensions × quality
    for &dim in DIMENSION_STEPS {
        let resized = img.resize(dim, dim, image::imageops::FilterType::Lanczos3);
        for &quality in QUALITY_STEPS {
            let mut buf = Cursor::new(Vec::new());
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
            if resized.write_with_encoder(encoder).is_err() {
                continue;
            }
            let encoded_bytes = buf.into_inner();
            if encoded_bytes.len() <= max_bytes {
                let b64 = engine.encode(&encoded_bytes);
                return Ok((b64, "image/jpeg".to_string()));
            }
        }
    }

    // Fallback: use smallest combo regardless
    let resized = img.resize(800, 800, image::imageops::FilterType::Lanczos3);
    let mut buf = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 35);
    resized
        .write_with_encoder(encoder)
        .map_err(|e| anyhow::anyhow!("JPEG encode failed: {e}"))?;
    let b64 = engine.encode(buf.into_inner());
    Ok((b64, "image/jpeg".to_string()))
}

/// True when `mime_type` names a video container Gemini accepts via inline_data.
pub fn is_supported_video(mime_type: &str) -> bool {
    matches!(
        mime_type,
        "video/mp4" | "video/quicktime" | "video/webm" | "video/x-matroska"
    )
}

/// Check if MIME type is a compressible image format.
pub fn is_compressible_image(mime_type: &str) -> bool {
    matches!(
        mime_type,
        "image/jpeg"
            | "image/jpg"
            | "image/png"
            | "image/webp"
            | "image/gif"
            | "image/bmp"
            | "image/tiff"
            | "image/heic"
            | "image/heif"
    )
}

/// Check if MIME type is HEIC/HEIF.
fn is_heic(mime_type: &str) -> bool {
    matches!(mime_type, "image/heic" | "image/heif")
}

/// Convert HEIC/HEIF to JPEG via macOS sips (20s timeout).
#[cfg(target_os = "macos")]
fn convert_heic_to_jpeg(heic_bytes: &[u8]) -> Result<Vec<u8>> {
    use std::process::Command;

    let dir = tempfile::tempdir()?;
    let input_path = dir.path().join("input.heic");
    let output_path = dir.path().join("output.jpg");
    std::fs::write(&input_path, heic_bytes)?;

    let status = Command::new("/usr/bin/sips")
        .args([
            "-s",
            "format",
            "jpeg",
            &input_path.to_string_lossy(),
            "--out",
            &output_path.to_string_lossy(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;

    if !status.success() {
        anyhow::bail!("sips exited with status {status}");
    }

    Ok(std::fs::read(&output_path)?)
}

#[cfg(not(target_os = "macos"))]
fn convert_heic_to_jpeg(_heic_bytes: &[u8]) -> Result<Vec<u8>> {
    anyhow::bail!("HEIC conversion requires macOS")
}

/// Infer MIME type from first bytes (magic bytes).
pub fn infer_mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 4 {
        return None;
    }
    if bytes[0] == 0xFF && bytes[1] == 0xD8 {
        return Some("image/jpeg");
    }
    if bytes[0] == 0x89 && &bytes[1..4] == b"PNG" {
        return Some("image/png");
    }
    if &bytes[..3] == b"GIF" {
        return Some("image/gif");
    }
    if &bytes[..4] == b"RIFF" && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes[0] == 0x42 && bytes[1] == 0x4D {
        return Some("image/bmp");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        if &bytes[8..12] == b"qt  " {
            return Some("video/quicktime");
        }
        return Some("video/mp4");
    }
    if bytes.len() >= 4 && bytes[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some("video/webm");
    }
    None
}

/// Validate and canonicalize base64 data. Returns None if invalid.
pub fn validate_base64(data: &str) -> Option<String> {
    // Strip whitespace (canonicalize)
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();

    let engine = base64::engine::general_purpose::STANDARD;
    if engine.decode(&cleaned).is_err() {
        return None;
    }

    Some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve one HTTP response with `Transfer-Encoding: chunked` (no
    /// Content-Length header at all) whose body is `total` bytes of chunks.
    /// Chunked encoding is exactly the shape where a Content-Length pre-check
    /// is blind and only a streamed running cap can stop an oversized body.
    async fn serve_chunked(total: usize) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut tmp = [0u8; 1024];
            let _ = sock.read(&mut tmp).await; // drain request headers
            let header =
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
            if sock.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            const CHUNK: usize = 64 * 1024;
            let payload = vec![b'x'; CHUNK];
            let mut sent = 0usize;
            while sent < total {
                let n = CHUNK.min(total - sent);
                let frame = format!("{n:x}\r\n");
                // A capped reader aborts mid-stream by dropping the
                // connection; treat write failure as the expected end.
                if sock.write_all(frame.as_bytes()).await.is_err()
                    || sock.write_all(&payload[..n]).await.is_err()
                    || sock.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
                sent += n;
            }
            let _ = sock.write_all(b"0\r\n\r\n").await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn read_body_capped_stops_chunked_body_with_no_content_length_at_cap() {
        // 4 MB body, 256 KB cap, no Content-Length. A buffer-then-check
        // implementation would slurp all 4 MB (or hang on an unbounded
        // stream); the streamed cap must abort shortly past 256 KB.
        let base = serve_chunked(4 * 1024 * 1024).await;
        let resp = reqwest::get(format!("{base}/big")).await.unwrap();
        assert_eq!(
            resp.content_length(),
            None,
            "test precondition: chunked response must not report a length"
        );
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_body_capped(resp, 256 * 1024, "test payload"),
        )
        .await
        .expect("capped read must abort mid-stream, not buffer the whole body")
        .expect_err("body over the cap must be rejected");
        assert!(
            err.to_string().contains("too large"),
            "error must explain the size rejection, got: {err}"
        );
    }

    #[tokio::test]
    async fn read_body_capped_returns_full_body_under_cap() {
        let base = serve_chunked(150 * 1024).await;
        let resp = reqwest::get(format!("{base}/ok")).await.unwrap();
        let body = read_body_capped(resp, 256 * 1024, "test payload")
            .await
            .expect("body under the cap must be returned intact");
        assert_eq!(body.len(), 150 * 1024);
        assert!(body.iter().all(|b| *b == b'x'));
    }

    #[test]
    fn parse_valid_data_uri() {
        let uri = "data:image/png;base64,abc123";
        let media = parse_data_uri(uri).unwrap();
        assert_eq!(media.mime_type, "image/png");
        assert_eq!(media.data, "abc123");
        assert!(media.filename.is_none());
    }

    #[test]
    fn parse_rejects_missing_prefix() {
        assert!(parse_data_uri("not-a-data-uri").is_err());
    }

    #[test]
    fn parse_rejects_missing_comma() {
        assert!(parse_data_uri("data:image/png;base64").is_err());
    }

    #[test]
    fn parse_rejects_missing_base64_marker() {
        assert!(parse_data_uri("data:text/plain,hello").is_err());
    }

    #[test]
    fn parse_rejects_empty_data() {
        assert!(parse_data_uri("data:image/png;base64,").is_err());
    }

    #[test]
    fn is_compressible_image_true() {
        assert!(is_compressible_image("image/jpeg"));
        assert!(is_compressible_image("image/png"));
        assert!(is_compressible_image("image/webp"));
        assert!(is_compressible_image("image/heic"));
    }

    #[test]
    fn is_compressible_image_false() {
        assert!(!is_compressible_image("image/svg+xml"));
        assert!(!is_compressible_image("application/pdf"));
        assert!(!is_compressible_image("text/plain"));
    }

    #[test]
    fn is_heic_detection() {
        assert!(is_heic("image/heic"));
        assert!(is_heic("image/heif"));
        assert!(!is_heic("image/jpeg"));
    }

    /// Magic-byte inference covers every format we surface to the LLM plus
    /// the unknown-input fallback. Collapsed to a table so adding a new
    /// format means appending one row, not adding a near-duplicate test.
    #[test]
    fn infer_mime_recognises_known_magic_bytes() {
        let cases: &[(&[u8], Option<&str>)] = &[
            (&[0xFF, 0xD8, 0xFF, 0xE0], Some("image/jpeg")),
            (&[0x89, b'P', b'N', b'G'], Some("image/png")),
            (b"GIF89a", Some("image/gif")),
            // RIFF…WEBP needs the 12-byte WEBP suffix to disambiguate from
            // RIFF/WAV — guard against a regression that accepts bare RIFF.
            (b"RIFF\0\0\0\0WEBP", Some("image/webp")),
            (&[0x42, 0x4D, 0, 0], Some("image/bmp")),
            // MP4 / QuickTime use the `ftyp` ISO base media file marker.
            (b"\0\0\0\0ftypqt  ", Some("video/quicktime")),
            (b"\0\0\0\0ftypisom", Some("video/mp4")),
            (&[0x1A, 0x45, 0xDF, 0xA3], Some("video/webm")),
            // Bare RIFF without WEBP must NOT be claimed as webp — covers
            // both the positive and the negative discrimination.
            (b"RIFF\0\0\0\0WAVEfmt ", None),
            // Unknown input falls back to None rather than guessing.
            (&[0x00, 0x00, 0x00, 0x00], None),
        ];
        for (bytes, expected) in cases {
            let got = infer_mime_from_bytes(bytes);
            assert_eq!(
                got, *expected,
                "infer_mime_from_bytes({bytes:02X?}) = {got:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn validate_base64_valid() {
        // Already-clean valid base64 is returned verbatim (decodes to "Hello").
        assert_eq!(validate_base64("SGVsbG8=").as_deref(), Some("SGVsbG8="));
    }

    #[test]
    fn validate_base64_with_whitespace() {
        let result = validate_base64("SGVs bG8=");
        assert!(result.is_some());
        assert_eq!(result.as_deref(), Some("SGVsbG8="));
    }

    #[test]
    fn validate_base64_invalid() {
        assert!(validate_base64("not!valid!base64!!!").is_none());
    }

    #[test]
    fn compress_small_image_passthrough() {
        // Create a tiny valid JPEG-like base64 that's under the limit
        let engine = base64::engine::general_purpose::STANDARD;
        // Smallest valid JPEG: FFD8FFD9 (start + end markers)
        let tiny_jpeg = vec![0xFF, 0xD8, 0xFF, 0xD9];
        let b64 = engine.encode(&tiny_jpeg);
        let (data, mime) =
            compress_image(&b64, "image/jpeg", 6 * 1024 * 1024).expect("under-limit image");
        // Small image should pass through unchanged
        assert_eq!(data, b64);
        assert_eq!(mime, "image/jpeg");
    }
}
