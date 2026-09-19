//! Shared image-bytes validation: sniff format, MIME allow-list, optional
//! full-pixel decode (catches CRC/IDAT corruption a header-only check
//! misses).

#![deny(clippy::indexing_slicing)]

use image::{ImageError, ImageFormat};

/// `image`-crate error message substrings classified as `Truncated`.
const TRUNCATED_NEEDLES: &[&str] = &[
    "unexpected eof",
    "end of stream",
    "unexpected end of data",
    "unexpected end",
];

/// Why `validate_image_bytes_with` rejected the input.
#[derive(Debug, thiserror::Error)]
pub enum ImageValidateError {
    #[error("image bytes are empty")]
    Empty,
    /// Format could not be sniffed (no recognised magic bytes).
    #[error("unsupported or unrecognised image format")]
    Unsupported,
    /// Header read failed because the file is shorter than expected.
    #[error("image bytes are truncated")]
    Truncated,
    /// PNG IDAT/IHDR or equivalent chunk CRC mismatch.
    #[error("image has bad chunk CRC")]
    BadCrc,
    /// Sniffed format is not on the allow-list (e.g. SVG, x-icon, TGA).
    #[error("image format is not in the allow-list")]
    WrongFormat,
    #[error("image decode failed: {0}")]
    Decode(String),
}

fn validate_inner(
    bytes: &[u8],
    validate_full_decode: bool,
) -> Result<(u32, u32, ImageFormat), ImageValidateError> {
    if bytes.is_empty() {
        return Err(ImageValidateError::Empty);
    }
    let format = image::guess_format(bytes).map_err(classify_image_error)?;
    if validate_full_decode {
        // The JPEG decoder (zune-jpeg) pads missing scan data instead of
        // erroring, so a truncated JPEG passes a full pixel decode; the
        // API still rejects it. Enforce marker-structure completeness.
        if format == ImageFormat::Jpeg && !jpeg_reaches_eoi(bytes) {
            return Err(ImageValidateError::Truncated);
        }
        let img = image::load_from_memory(bytes).map_err(classify_image_error)?;
        return Ok((img.width(), img.height(), format));
    }
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| classify_io_kind(e.kind(), e.to_string()))?
        .into_dimensions()
        .map_err(classify_image_error)?;
    Ok((w, h, format))
}

fn allowlist_mime(format: ImageFormat) -> Result<&'static str, ImageValidateError> {
    match format {
        ImageFormat::Png => Ok("image/png"),
        ImageFormat::Jpeg => Ok("image/jpeg"),
        ImageFormat::Gif => Ok("image/gif"),
        ImageFormat::WebP => Ok("image/webp"),
        ImageFormat::Bmp => Ok("image/bmp"),
        ImageFormat::Tiff => Ok("image/tiff"),
        _ => Err(ImageValidateError::WrongFormat),
    }
}

/// Validate `bytes` decode as an allow-listed image
/// (PNG/JPEG/GIF/WebP/BMP/TIFF). When `validate_full_decode` is `true`,
/// runs a full pixel decode (catches CRC-corrupt PNGs); otherwise parses
/// only the header.
pub fn validate_image_bytes_with(
    bytes: &[u8],
    validate_full_decode: bool,
) -> Result<(u32, u32, &'static str), ImageValidateError> {
    let (w, h, format) = validate_inner(bytes, validate_full_decode)?;
    let mime = allowlist_mime(format)?;
    Ok((w, h, mime))
}

/// Default full-decode validation; catches magic-byte forgeries and
/// CRC-corrupt PNGs.
pub fn validate_image_bytes(bytes: &[u8]) -> Result<(u32, u32, &'static str), ImageValidateError> {
    validate_image_bytes_with(bytes, true)
}

/// Unrestricted dimension probe — accepts any format the `image` crate
/// can identify (TGA, ICO, PNM, HDR, Farbfeld, etc.). Inference-bound
/// paths MUST use [`validate_image_bytes_with`] (allow-list enforced).
pub fn validate_image_bytes_unrestricted(
    bytes: &[u8],
    validate_full_decode: bool,
) -> Result<(u32, u32, ImageFormat), ImageValidateError> {
    validate_inner(bytes, validate_full_decode)
}

/// Walk the JPEG marker structure and report whether a top-level EOI
/// (`FFD9`) is reached. Truncated files end inside a segment or the
/// entropy-coded stream and never reach it.
///
/// Structure-only (no pixel decode): length-prefixed segments are skipped
/// by their declared length — so an `FFD9` inside e.g. an EXIF thumbnail
/// does not count — and entropy-coded data after SOS is scanned with
/// byte-stuffing awareness (`FF00` literal, `FFD0`-`FFD7` restart markers).
/// Trailing bytes after the first top-level EOI (EXIF trailers, motion
/// photos) are ignored.
///
/// Stray non-`FF` bytes at marker positions (broken EXIF/APPn writers) are
/// skipped rather than rejected, mirroring libjpeg's `next_marker` — every
/// decoder in the accept chain (libjpeg/PIL, zune-jpeg, image-rs) reads
/// such files, so rejecting them here would drop images the API accepts.
/// Truncation detection is unaffected: a cut file still runs off the
/// buffer end without a top-level EOI.
///
/// Assumes Huffman byte-stuffing; arithmetic-coded entropy data (T.81
/// Annex D, which none of our decoders or the inference API accept) may
/// be false-rejected.
pub fn jpeg_reaches_eoi(bytes: &[u8]) -> bool {
    let [0xFF, 0xD8, ..] = bytes else {
        return false;
    };
    let mut i = 2;
    loop {
        // Find the next marker: skip stray garbage, then FF fill bytes.
        while bytes.get(i).is_some_and(|&b| b != 0xFF) {
            i += 1;
        }
        while bytes.get(i).copied() == Some(0xFF) {
            i += 1;
        }
        let Some(&marker) = bytes.get(i) else {
            return false;
        };
        i += 1;
        match marker {
            // Not a marker (stuffed/stray `FF00`): keep scanning.
            0x00 => {}
            0xD9 => return true, // EOI
            // Standalone markers without a length field.
            0x01 | 0xD0..=0xD7 => {}
            0xDA => {
                // SOS: skip the length-prefixed header, then scan the
                // entropy-coded stream for the next real marker.
                let Some(next) = skip_segment(bytes, i) else {
                    return false;
                };
                i = next;
                loop {
                    while bytes.get(i).is_some_and(|&b| b != 0xFF) {
                        i += 1;
                    }
                    match bytes.get(i + 1).copied() {
                        // Byte-stuffed FF or fill byte: still entropy data.
                        Some(0x00) => i += 2,
                        Some(0xFF) => i += 1,
                        // Restart marker: entropy data continues after it.
                        Some(0xD0..=0xD7) => i += 2,
                        Some(_) => break,
                        None => return false,
                    }
                }
            }
            _ => {
                let Some(next) = skip_segment(bytes, i) else {
                    return false;
                };
                i = next;
            }
        }
    }
}

/// Skip a length-prefixed JPEG segment starting at its 2-byte length field.
/// Returns the offset just past the segment, or `None` if it runs off the end.
fn skip_segment(bytes: &[u8], at: usize) -> Option<usize> {
    let len_bytes = bytes.get(at..at + 2)?;
    let [hi, lo] = len_bytes else {
        return None;
    };
    let len = usize::from(*hi) << 8 | usize::from(*lo);
    if len < 2 {
        return None;
    }
    let end = at.checked_add(len)?;
    (end <= bytes.len()).then_some(end)
}

/// Walk PNG chunks to an `IEND` chunk, verifying each chunk's CRC along
/// the way (no pixel decode). This is exactly the inference API's
/// per-request `validate_png_chunk_crcs` gate, so a pass here means the
/// server's PNG validation passes too — and a reject here is never a
/// false drop, because the server would reject the same bytes.
pub fn png_structurally_valid(bytes: &[u8]) -> bool {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    let n = bytes.len();
    if !bytes.starts_with(PNG_SIG) {
        return false;
    }
    let mut i = PNG_SIG.len();
    // Each chunk: 4-byte length, 4-byte type, data, 4-byte CRC (over
    // type + data).
    while let Some(header) = bytes.get(i..i + 8) {
        let [l0, l1, l2, l3, t0, t1, t2, t3] = header else {
            return false;
        };
        let len = u32::from_be_bytes([*l0, *l1, *l2, *l3]) as usize;
        let is_iend = [*t0, *t1, *t2, *t3] == *b"IEND";
        let data_start = i + 8;
        let Some(data_end) = data_start.checked_add(len) else {
            return false;
        };
        let Some(end) = data_end.checked_add(4) else {
            return false;
        };
        if end > n {
            return false;
        }
        let Some(crc) = bytes.get(data_end..data_end + 4) else {
            return false;
        };
        let [c0, c1, c2, c3] = crc else {
            return false;
        };
        let expected = u32::from_be_bytes([*c0, *c1, *c2, *c3]);
        let Some(crc_span) = bytes.get(i + 4..data_end) else {
            return false;
        };
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(crc_span);
        if hasher.finalize() != expected {
            return false;
        }
        if is_iend {
            return true;
        }
        i = end;
    }
    false
}

/// WebP: the RIFF header declares the total payload size at bytes 4..8;
/// truncation leaves the buffer shorter than declared. An optional pad
/// byte (odd riff size) and trailing garbage are tolerated.
pub fn webp_riff_complete(bytes: &[u8]) -> bool {
    let Some(header) = bytes.get(..12) else {
        return false;
    };
    if header.get(..4) != Some(&b"RIFF"[..]) || header.get(8..12) != Some(&b"WEBP"[..]) {
        return false;
    }
    let Some(size_bytes) = header.get(4..8) else {
        return false;
    };
    let [s0, s1, s2, s3] = size_bytes else {
        return false;
    };
    let riff_size = u32::from_le_bytes([*s0, *s1, *s2, *s3]) as usize;
    riff_size
        .checked_add(8)
        .is_some_and(|end| end <= bytes.len())
}

/// Structural validity walk for a known `format`. Formats without a
/// dedicated walk return `true` — their decoders already reject
/// truncation strictly.
pub fn format_structurally_complete(format: ImageFormat, bytes: &[u8]) -> bool {
    match format {
        ImageFormat::Jpeg => jpeg_reaches_eoi(bytes),
        ImageFormat::Png => png_structurally_valid(bytes),
        ImageFormat::WebP => webp_riff_complete(bytes),
        _ => true,
    }
}

/// [`format_structurally_complete`] on the sniffed format; unsniffable
/// bytes fail — the inference API rejects them regardless.
pub fn image_structurally_complete(bytes: &[u8]) -> bool {
    match image::guess_format(bytes) {
        Ok(format) => format_structurally_complete(format, bytes),
        Err(_) => false,
    }
}

/// Decode-bomb guard: reject oversized inputs before full pixel decode.
const MAX_TRANSCODE_DECODE_PIXELS: u64 = 16_000_000;

/// Upscale tiny inputs so the PNG clears the backend `MIN_IMAGE_PIXELS`
/// (512) floor; a native PNG below it is rejected, not upscaled, server-side.
/// Matches the backend's `ICO_MIN_UPSCALE_DIMENSION`.
const TRANSCODE_MIN_UPSCALE_SIDE: u32 = 128;

/// Formats we re-encode as PNG before send. Engines only sample JPEG/PNG/WebP;
/// the backend rejects GIF/BMP/TIFF (it transcodes ICO server-side, not these).
fn is_client_transcode_format(format: ImageFormat) -> bool {
    matches!(
        format,
        ImageFormat::Ico | ImageFormat::Gif | ImageFormat::Bmp | ImageFormat::Tiff
    )
}

/// Whether `bytes` needs client-side PNG conversion (GIF/BMP/TIFF/ICO).
/// Engine-native JPG/PNG/WebP return `false`.
pub fn needs_endpoint_transcode(bytes: &[u8]) -> bool {
    matches!(
        image::guess_format(bytes),
        Ok(fmt) if is_client_transcode_format(fmt)
    )
}

/// Transcode ICO/GIF/BMP/TIFF to PNG. Returns `None` for already-native
/// (JPG/PNG/WebP) or unrecognised input (caller keeps the original bytes);
/// `Some(Err)` on decode failure. Tiny inputs are upscaled (see
/// [`TRANSCODE_MIN_UPSCALE_SIDE`]).
pub fn transcode_to_endpoint_png(bytes: &[u8]) -> Option<Result<Vec<u8>, ImageValidateError>> {
    let format = image::guess_format(bytes).ok()?;
    if !is_client_transcode_format(format) {
        return None;
    }
    Some(decode_to_png(bytes, format))
}

fn decode_to_png(bytes: &[u8], format: ImageFormat) -> Result<Vec<u8>, ImageValidateError> {
    // Probe dimensions from the header before decoding the full bitmap.
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| classify_io_kind(e.kind(), e.to_string()))?
        .into_dimensions()
        .map_err(classify_image_error)?;
    if (w as u64) * (h as u64) > MAX_TRANSCODE_DECODE_PIXELS {
        return Err(ImageValidateError::Decode(format!(
            "{format:?} {w}x{h} exceeds {MAX_TRANSCODE_DECODE_PIXELS} px decode limit"
        )));
    }
    let mut img = image::load_from_memory(bytes).map_err(classify_image_error)?;
    // Upscale the shorter side to TRANSCODE_MIN_UPSCALE_SIDE (aspect preserved),
    // but only if the post-resize pixel count still fits the decode budget: a
    // thin ultra-wide frame can pass the header check yet blow it after scaling.
    let shortest = img.width().min(img.height());
    if shortest > 0 && shortest < TRANSCODE_MIN_UPSCALE_SIDE {
        let scale = TRANSCODE_MIN_UPSCALE_SIDE as f32 / shortest as f32;
        let new_w = ((img.width() as f32 * scale).round() as u32).max(TRANSCODE_MIN_UPSCALE_SIDE);
        let new_h = ((img.height() as f32 * scale).round() as u32).max(TRANSCODE_MIN_UPSCALE_SIDE);
        let post_pixels = (new_w as u64).saturating_mul(new_h as u64);
        if post_pixels <= MAX_TRANSCODE_DECODE_PIXELS {
            img = img.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3);
        }
        // else: keep original size rather than allocate an unbounded bitmap.
    }
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
        .map_err(classify_image_error)?;
    Ok(out)
}

fn classify_image_error(e: ImageError) -> ImageValidateError {
    match &e {
        ImageError::IoError(io) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
            return ImageValidateError::Truncated;
        }
        ImageError::Unsupported(_) => return ImageValidateError::Unsupported,
        _ => {}
    }
    let msg = e.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("crc") {
        ImageValidateError::BadCrc
    } else if TRUNCATED_NEEDLES.iter().any(|n| lower.contains(n)) {
        ImageValidateError::Truncated
    } else {
        ImageValidateError::Decode(msg)
    }
}

fn classify_io_kind(kind: std::io::ErrorKind, msg: String) -> ImageValidateError {
    if kind == std::io::ErrorKind::UnexpectedEof {
        return ImageValidateError::Truncated;
    }
    let lower = msg.to_ascii_lowercase();
    if TRUNCATED_NEEDLES.iter().any(|n| lower.contains(n)) {
        ImageValidateError::Truncated
    } else {
        ImageValidateError::Decode(msg)
    }
}

#[cfg(test)]
#[path = "image_validate_tests.rs"]
mod tests;
