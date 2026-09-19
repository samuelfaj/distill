// Modified for Distill by Samuel Fajreldines, 2026.
//! Magic-byte metadata inspection shared by read tools.

use std::path::Path;

/// Metadata extracted from file bytes via magic-byte inspection.
#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: usize,
    pub mime_type: String,
}

impl FileMetadata {
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }

    pub fn is_pdf(&self) -> bool {
        self.mime_type == "application/pdf"
    }
}

const PDF_MAGIC: &[u8; 5] = b"%PDF-";

pub(crate) fn is_pdf_magic(bytes: &[u8]) -> bool {
    bytes.get(..5) == Some(PDF_MAGIC.as_slice())
}

/// Infer file metadata (MIME type, extension) from raw bytes using magic-byte inspection.
pub fn bytes_to_metadata(file_bytes: &[u8]) -> Result<FileMetadata, distill_tool_runtime::ToolError> {
    let size = file_bytes.len();
    let data = infer::get(file_bytes).ok_or_else(|| {
        distill_tool_runtime::ToolError::invalid_arguments("failed to infer file type from magic bytes")
    })?;

    Ok(FileMetadata {
        size,
        mime_type: data.mime_type().to_string(),
    })
}

/// Whether `read_file` may embed `bytes` as a conversation image.
///
/// Adobe SVG often starts with a PNG thumbnail. `infer` sniffs that prefix as
/// `image/png`, and a structurally incomplete thumbnail (no `IEND`) 400s later
/// turns as `invalid_image`. `.svg` / `image/svg+xml`, and an incomplete PNG
/// prefix followed by SVG markup, stay on the text path. GIF, JPEG, and a
/// complete PNG still embed even if their bytes contain `<svg`.
pub fn should_embed_as_conversation_image(path: &Path, bytes: &[u8], mime: &str) -> bool {
    if !mime.starts_with("image/") {
        return false;
    }
    if is_svg_path_or_mime(path, mime) || looks_like_truncated_png_with_svg_suffix(bytes) {
        return false;
    }
    true
}

fn is_svg_path_or_mime(path: &Path, mime: &str) -> bool {
    mime.eq_ignore_ascii_case("image/svg+xml")
        || path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
}

/// Adobe Illustrator SVG: incomplete PNG thumbnail, then XML. Other image
/// codecs (GIF comment, JPEG) can contain the letters `<svg` without being SVG.
fn looks_like_truncated_png_with_svg_suffix(bytes: &[u8]) -> bool {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return false;
    }
    let suffix = skip_complete_png_chunks(bytes);
    !suffix.is_empty() && extract_svg_text(bytes).is_some()
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Bytes after every complete PNG chunk. Adobe SVG is a PNG thumbnail plus
/// XML; `<svg` also appears inside compressed `IDAT`, so a whole-file scan
/// would treat a real PNG as markup and leak `\u{FFFD}` into the text read.
fn skip_complete_png_chunks(bytes: &[u8]) -> &[u8] {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return bytes;
    }
    let mut i = PNG_SIGNATURE.len();
    loop {
        let Some(header_end) = i.checked_add(8) else {
            break;
        };
        let Some(&[l0, l1, l2, l3, t0, t1, t2, t3]) = bytes.get(i..header_end) else {
            break;
        };
        let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
        let is_iend = [t0, t1, t2, t3] == *b"IEND";
        let Some(chunk_end) = header_end.checked_add(4).and_then(|n| n.checked_add(len)) else {
            break;
        };
        if chunk_end > bytes.len() {
            break;
        }
        i = chunk_end;
        if is_iend {
            break;
        }
    }
    // Unread tail: complete chunks stay behind `i`, which never exceeds `len`.
    bytes.get(i..).unwrap_or(&[])
}

/// SVG markup, including after a leading PNG thumbnail. `None` if no `<svg` tag
/// is present (a truncated preview-only Adobe file).
pub fn extract_svg_text(bytes: &[u8]) -> Option<String> {
    let suffix = skip_complete_png_chunks(bytes);
    if let Some(xml) = find_ignore_ascii_case(suffix, b"<?xml") {
        let text = std::str::from_utf8(suffix.get(xml..)?).ok()?;
        if text.contains("<svg") || text.contains("<SVG") {
            return Some(text.to_owned());
        }
    }
    let svg = find_ignore_ascii_case(suffix, b"<svg")?;
    std::str::from_utf8(suffix.get(svg..)?)
        .ok()
        .map(str::to_owned)
}

fn find_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// 1×1 PNG whose `IDAT` payload contains the literal `<svg` bytes (CRC-valid).
#[cfg(test)]
pub(crate) fn png_with_svg_bytes_in_idat() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x78, 0x78, 0x78,
        0x3c, 0x73, 0x76, 0x67, 0x59, 0x59, 0x59, 0x59, 0xd7, 0xd3, 0x95, 0x4d, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}

#[cfg(test)]
pub(crate) fn truncated_png_prefix() -> Vec<u8> {
    let mut png = png_with_svg_bytes_in_idat();
    let iend = png.windows(4).rposition(|w| w == b"IEND").expect("IEND");
    png.truncate(iend.saturating_sub(4));
    assert!(
        !crate::util::image_validate::png_structurally_valid(&png),
        "precondition: thumbnail has no IEND"
    );
    png
}

#[cfg(test)]
pub(crate) fn truncated_png_then_svg() -> Vec<u8> {
    let mut png = truncated_png_prefix();
    png.extend_from_slice(
        br#"<?xml version="1.0" encoding="utf-8"?>
<!-- Generator: Adobe Illustrator 27.0.0, SVG Export Plug-In -->
<svg xmlns="http://www.w3.org/2000/svg" width="100" height="40">
  <text x="0" y="20">baidu</text>
</svg>
"#,
    );
    png
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn is_pdf_magic_valid() {
        assert!(is_pdf_magic(b"%PDF-1.7 rest of file"));
        assert!(is_pdf_magic(b"%PDF-2.0"));
    }

    #[test]
    fn is_pdf_magic_invalid() {
        assert!(!is_pdf_magic(b"not a pdf"));
        assert!(!is_pdf_magic(b"%PD"));
        assert!(!is_pdf_magic(b""));
    }

    #[test]
    fn file_metadata_is_pdf() {
        let meta = FileMetadata {
            size: 100,
            mime_type: "application/pdf".to_string(),
        };
        assert!(meta.is_pdf());
        assert!(!meta.is_image());
    }

    #[test]
    fn file_metadata_image_is_not_pdf() {
        let meta = FileMetadata {
            size: 100,
            mime_type: "image/png".to_string(),
        };
        assert!(!meta.is_pdf());
        assert!(meta.is_image());
    }

    #[test]
    fn metadata_is_image_and_is_pdf_are_exclusive() {
        let pdf_meta = FileMetadata {
            size: 0,
            mime_type: "application/pdf".to_string(),
        };
        assert!(pdf_meta.is_pdf());
        assert!(!pdf_meta.is_image());

        let img_meta = FileMetadata {
            size: 0,
            mime_type: "image/jpeg".to_string(),
        };
        assert!(img_meta.is_image());
        assert!(!img_meta.is_pdf());
    }

    #[test]
    fn adobe_png_prefix_svg_is_sniffed_as_png_but_not_embedded() {
        let bytes = truncated_png_then_svg();
        let meta = bytes_to_metadata(&bytes).expect("infer sees the PNG prefix");
        assert_eq!(meta.mime_type, "image/png");
        assert!(meta.is_image());
        assert!(!should_embed_as_conversation_image(
            Path::new("baidu.svg"),
            &bytes,
            &meta.mime_type
        ));
        let text = extract_svg_text(&bytes).expect("SVG markup after the thumbnail");
        assert!(text.contains("<svg"));
        assert!(text.contains("baidu"));
        assert!(!text.as_bytes().starts_with(b"\x89PNG"));
        assert!(!text.contains('\u{FFFD}'));
    }

    #[test]
    fn svg_extension_without_markup_fails_closed() {
        let png = truncated_png_prefix();
        assert!(!should_embed_as_conversation_image(
            Path::new("preview.svg"),
            &png,
            "image/png"
        ));
        assert!(extract_svg_text(&png).is_none());
    }

    #[test]
    fn complete_png_still_embeds_even_when_idat_contains_svg_bytes() {
        let png = png_with_svg_bytes_in_idat();
        assert!(
            crate::util::image_validate::png_structurally_valid(&png),
            "precondition: fixture is a complete PNG"
        );
        assert!(png.windows(4).any(|w| w == b"<svg"));
        assert!(extract_svg_text(&png).is_none());
        assert!(should_embed_as_conversation_image(
            Path::new("icon.png"),
            &png,
            "image/png"
        ));
    }

    #[test]
    fn svg_mime_never_embeds() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert!(!should_embed_as_conversation_image(
            Path::new("logo.xml"),
            svg,
            "image/svg+xml"
        ));
        assert!(!should_embed_as_conversation_image(
            &PathBuf::from("Logo.SVG"),
            svg,
            "text/plain"
        ));
    }

    /// GIF89a plus trailing `<svg` bytes. A whole-file markup scan would refuse
    /// embed; only an incomplete PNG prefix may do that.
    fn gif_with_trailing_svg_bytes() -> Vec<u8> {
        let mut gif = vec![
            b'G', b'I', b'F', b'8', b'9', b'a', 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x3b,
        ];
        gif.extend_from_slice(br#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#);
        gif
    }

    #[test]
    fn gif_with_svg_bytes_still_embeds() {
        let gif = gif_with_trailing_svg_bytes();
        let meta = bytes_to_metadata(&gif).expect("infer sees the GIF header");
        assert_eq!(meta.mime_type, "image/gif");
        assert!(extract_svg_text(&gif).is_some());
        assert!(should_embed_as_conversation_image(
            Path::new("icon.gif"),
            &gif,
            &meta.mime_type
        ));
    }
}
