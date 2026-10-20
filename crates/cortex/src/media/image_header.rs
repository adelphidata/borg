//! Header-only image dimension parsing.
//!
//! Reads width and height from the leading bytes of a PNG, JPEG, GIF or WebP
//! without decoding pixels — and, for base64 payloads, without decoding more
//! than a bounded prefix of the string. A multi-MB screenshot costs a few
//! hundred bytes of work, which is what lets the outbound-image ceiling in the
//! LLM transport inspect every image in history on every request.

use base64::Engine;

use crate::constants::IMAGE_HEADER_B64_PREFIX_CHARS;

/// Pixel `(width, height)` of an encoded image, read from its header only.
/// Returns `None` for an unknown container, a truncated header, or a JPEG
/// whose frame header is not present in `bytes`.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dimensions(bytes)
        .or_else(|| jpeg_dimensions(bytes))
        .or_else(|| gif_dimensions(bytes))
        .or_else(|| webp_dimensions(bytes))
}

/// [`image_dimensions`] over a base64 payload. Decodes at most
/// [`IMAGE_HEADER_B64_PREFIX_CHARS`] characters (rounded down to a whole
/// base64 quantum so a mid-string cut never trips padding validation).
pub fn image_dimensions_from_base64(data: &str) -> Option<(u32, u32)> {
    let data = data.trim();
    let data = if data.len() > IMAGE_HEADER_B64_PREFIX_CHARS {
        let cut = IMAGE_HEADER_B64_PREFIX_CHARS - IMAGE_HEADER_B64_PREFIX_CHARS % 4;
        data.get(..cut)?
    } else {
        data
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    image_dimensions(&bytes)
}

const PNG_SIG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// PNG: width/height are two big-endian u32s at bytes 16..24 (right after
/// the 8-byte signature and the `IHDR` chunk's length + type).
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !bytes.starts_with(PNG_SIG) {
        return None;
    }
    Some((be_u32(&bytes[16..20]), be_u32(&bytes[20..24])))
}

/// JPEG: walk the marker segments from `SOI` until a start-of-frame marker
/// (`SOF0`..`SOF15`, excluding `DHT`/`JPG`/`DAC`), whose payload carries
/// precision, height and width.
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut pos = 2;
    loop {
        // Fill bytes before a marker are legal.
        while *bytes.get(pos)? == 0xFF {
            pos += 1;
        }
        let marker = *bytes.get(pos)?;
        pos += 1;
        match marker {
            0xC0..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => {
                // length(2) precision(1) height(2) width(2)
                let height = be_u16(bytes.get(pos + 3..pos + 5)?);
                let width = be_u16(bytes.get(pos + 5..pos + 7)?);
                return Some((u32::from(width), u32::from(height)));
            }
            // Standalone markers carry no length field.
            0x01 | 0xD0..=0xD8 => {}
            // Start of scan or end of image before any frame header: no dimensions.
            0xD9 | 0xDA => return None,
            _ => {
                let len = usize::from(be_u16(bytes.get(pos..pos + 2)?));
                pos = pos.checked_add(len)?;
            }
        }
    }
}

/// GIF: little-endian u16 width/height at bytes 6..10 of the header.
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 10 || !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return None;
    }
    Some((
        u32::from(le_u16(&bytes[6..8])),
        u32::from(le_u16(&bytes[8..10])),
    ))
}

/// WebP: `RIFF….WEBP` then one of three first chunks, each with its own
/// dimension encoding (`VP8 ` lossy, `VP8L` lossless, `VP8X` extended).
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        b"VP8 " => Some((
            u32::from(le_u16(&bytes[26..28]) & 0x3FFF),
            u32::from(le_u16(&bytes[28..30]) & 0x3FFF),
        )),
        b"VP8L" => {
            let b = &bytes[21..25];
            let width = 1 + ((u32::from(b[1] & 0x3F) << 8) | u32::from(b[0]));
            let height = 1
                + ((u32::from(b[3] & 0x0F) << 10)
                    | (u32::from(b[2]) << 2)
                    | (u32::from(b[1] & 0xC0) >> 6));
            Some((width, height))
        }
        b"VP8X" => Some((1 + le_u24(&bytes[24..27]), 1 + le_u24(&bytes[27..30]))),
        _ => None,
    }
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le_u24(b: &[u8]) -> u32 {
    u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16)
}

#[cfg(test)]
pub(crate) mod test_images {
    //! Minimal valid-enough headers for the dimension parsers; the pixel
    //! data that follows is irrelevant to a header read.

    /// A PNG signature + `IHDR` chunk announcing `w`×`h`, padded so the file
    /// is longer than the header window a real screenshot would have.
    pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&w.to_be_bytes());
        bytes.extend_from_slice(&h.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0u8; 64]);
        bytes
    }

    /// A JPEG with `app_segment_len` bytes of APP1 (EXIF-shaped) padding
    /// ahead of the frame header, using the given SOF marker.
    pub(crate) fn jpeg(w: u16, h: u16, sof_marker: u8, app_segment_len: usize) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8];
        // A segment length is a u16, so long padding spans several APP1s.
        let mut remaining = app_segment_len;
        while remaining > 0 {
            let chunk = remaining.min(60_000);
            bytes.extend_from_slice(&[0xFF, 0xE1]);
            bytes.extend_from_slice(&((chunk + 2) as u16).to_be_bytes());
            bytes.extend(std::iter::repeat_n(0x42, chunk));
            remaining -= chunk;
        }
        bytes.extend_from_slice(&[0xFF, sof_marker]);
        bytes.extend_from_slice(&17u16.to_be_bytes());
        bytes.push(8);
        bytes.extend_from_slice(&h.to_be_bytes());
        bytes.extend_from_slice(&w.to_be_bytes());
        bytes.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
        bytes
    }

    pub(crate) fn gif(w: u16, h: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&w.to_le_bytes());
        bytes.extend_from_slice(&h.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes
    }

    pub(crate) fn webp_vp8(w: u16, h: u16) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBPVP8 ");
        bytes.extend_from_slice(&20u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0x9D, 0x01, 0x2A]);
        bytes.extend_from_slice(&w.to_le_bytes());
        bytes.extend_from_slice(&h.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        bytes
    }

    pub(crate) fn webp_vp8x(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBPVP8X");
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        bytes.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        bytes.extend_from_slice(&[0u8; 8]);
        bytes
    }

    pub(crate) fn b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::test_images::*;
    use super::*;

    /// `(case name, encoded bytes, expected dimensions)`.
    type DimensionCase = (&'static str, Vec<u8>, Option<(u32, u32)>);

    #[test]
    fn reads_dimensions_from_each_container_header() {
        let cases: Vec<DimensionCase> = vec![
            ("png", png(2880, 1800), Some((2880, 1800))),
            (
                "jpeg sof0 no app segments",
                jpeg(640, 480, 0xC0, 0),
                Some((640, 480)),
            ),
            (
                "jpeg sof2 progressive behind 3 KB exif",
                jpeg(4032, 3024, 0xC2, 3000),
                Some((4032, 3024)),
            ),
            (
                "jpeg sof1 extended sequential",
                jpeg(100, 50, 0xC1, 0),
                Some((100, 50)),
            ),
            ("gif", gif(320, 200), Some((320, 200))),
            ("webp vp8 lossy", webp_vp8(1456, 816), Some((1456, 816))),
            (
                "webp vp8x extended",
                webp_vp8x(2500, 1600),
                Some((2500, 1600)),
            ),
            (
                "jpeg truncated before sof",
                jpeg(10, 10, 0xC0, 0)[..6].to_vec(),
                None,
            ),
            (
                "jpeg scan before any frame header",
                vec![0xFF, 0xD8, 0xFF, 0xDA, 0, 2],
                None,
            ),
            ("png truncated header", png(1, 1)[..20].to_vec(), None),
            ("not an image", b"hello world, not pixels".to_vec(), None),
            ("empty", Vec::new(), None),
        ];
        for (name, bytes, expected) in cases {
            assert_eq!(image_dimensions(&bytes), expected, "{name}");
        }
    }

    #[test]
    fn webp_lossless_decodes_14_bit_minus_one_fields() {
        // VP8L packs (width-1) in 14 bits then (height-1) in 14 bits after a
        // 0x2F signature byte. 1456x816 → 1455 = 0x5AF, 815 = 0x32F.
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&40u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBPVP8L");
        bytes.extend_from_slice(&20u32.to_le_bytes());
        bytes.push(0x2F);
        let packed: u32 = 1455 | (815 << 14);
        bytes.extend_from_slice(&packed.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        assert_eq!(image_dimensions(&bytes), Some((1456, 816)));
    }

    #[test]
    fn base64_prefix_read_handles_whitespace_padding_and_oversized_payloads() {
        assert_eq!(
            image_dimensions_from_base64(&format!("  {}\n", b64(&png(2880, 1800)))),
            Some((2880, 1800))
        );
        // A multi-MB screenshot: only the header window is decoded, and the
        // cut must land on a base64 quantum so the decoder does not reject it.
        let mut big = png(3024, 1964);
        big.extend(std::iter::repeat_n(0u8, 400_000));
        assert_eq!(image_dimensions_from_base64(&b64(&big)), Some((3024, 1964)));
        // A JPEG whose frame header sits past the decoded window is unreadable.
        let far = jpeg(100, 100, 0xC0, 200_000);
        assert_eq!(image_dimensions_from_base64(&b64(&far)), None);
        assert_eq!(image_dimensions_from_base64("not base64 %%%"), None);
    }
}
