use std::io::Cursor;

use anyhow::Result;
use base64::Engine;
use tracing::warn;

use crate::types::{ContentPart, MediaData};

/// Max side length steps to try, descending.
const DIMENSION_STEPS: &[u32] = &[2048, 1536, 1280, 1024, 800];

/// JPEG quality levels to try, descending.
const QUALITY_STEPS: &[u8] = &[85, 75, 65, 55, 45, 35];

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

    // Validate and decode base64
    let validated =
        validate_base64(base64_data).ok_or_else(|| anyhow::anyhow!("invalid base64 data"))?;
    let raw_bytes = engine.decode(&validated)?;

    // Handle HEIC/HEIF
    let (bytes, _effective_mime) = if is_heic(mime_type) {
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

    // If already small enough, return as-is
    if bytes.len() <= max_bytes {
        return Ok((base64_data.to_string(), mime_type.to_string()));
    }

    // Load image
    let img = image::ImageReader::new(Cursor::new(&bytes))
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

/// Compress all ImageBase64 parts in-place. Logs warnings on failure, keeps originals.
pub fn compress_content_parts(parts: &mut [ContentPart], max_bytes: usize) {
    for part in parts.iter_mut() {
        if let ContentPart::ImageBase64 { media } = part {
            if !is_compressible_image(&media.mime_type) {
                continue;
            }
            match compress_image(&media.data, &media.mime_type, max_bytes) {
                Ok((new_data, new_mime)) => {
                    media.data = new_data;
                    media.mime_type = new_mime;
                }
                Err(e) => {
                    warn!("Image compression failed, keeping original: {e}");
                }
            }
        }
    }
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
    None
}

/// Validate and canonicalize base64 data. Returns None if invalid.
pub fn validate_base64(data: &str) -> Option<String> {
    // Strip whitespace (canonicalize)
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();

    // Check valid base64 charset
    let engine = base64::engine::general_purpose::STANDARD;
    if engine.decode(&cleaned).is_err() {
        return None;
    }

    Some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn infer_mime_jpeg() {
        assert_eq!(
            infer_mime_from_bytes(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
    }

    #[test]
    fn infer_mime_png() {
        assert_eq!(
            infer_mime_from_bytes(&[0x89, b'P', b'N', b'G']),
            Some("image/png")
        );
    }

    #[test]
    fn infer_mime_gif() {
        assert_eq!(infer_mime_from_bytes(b"GIF89a"), Some("image/gif"));
    }

    #[test]
    fn infer_mime_unknown() {
        assert_eq!(infer_mime_from_bytes(&[0x00, 0x00, 0x00, 0x00]), None);
    }

    #[test]
    fn validate_base64_valid() {
        assert!(validate_base64("SGVsbG8=").is_some());
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
        let result = compress_image(&b64, "image/jpeg", 6 * 1024 * 1024);
        assert!(result.is_ok());
        let (data, mime) = result.unwrap_or_default();
        // Small image should pass through unchanged
        assert_eq!(data, b64);
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn compress_content_parts_leaves_text() {
        let mut parts = vec![
            ContentPart::Text("hello".into()),
            ContentPart::Text("world".into()),
        ];
        compress_content_parts(&mut parts, 6 * 1024 * 1024);
        assert!(matches!(&parts[0], ContentPart::Text(t) if t == "hello"));
        assert!(matches!(&parts[1], ContentPart::Text(t) if t == "world"));
    }

    #[test]
    fn media_config_defaults() {
        let config = crate::config::MediaConfig::default();
        assert_eq!(config.max_image_bytes, 6 * 1024 * 1024);
        assert!(config.compression_enabled);
        assert_eq!(config.max_dimension_px, 2048);
    }
}
