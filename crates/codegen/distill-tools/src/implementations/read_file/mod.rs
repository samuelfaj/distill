// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared image/PDF/metadata helpers for read tools (distill, etc.).

pub mod image;
pub mod metadata;
pub mod pdf;
pub mod pptx;

pub use metadata::{
    FileMetadata, bytes_to_metadata, extract_svg_text, should_embed_as_conversation_image,
};
pub use pdf::{PDF_MAX_PAGES_PER_READ, parse_page_range};

pub use image::{CompressImageError, compress_image_for_conversation, image_read_output};
pub use pdf::{
    MAX_PDF_BYTES, PDF_PROCESS_TIMEOUT, extract_pdf_plain_text_cursor, is_pdf_file, make_test_pdf,
    run_document_extraction,
};

// Internal helpers still used only inside this crate.
pub(crate) use pdf::{handle_pdf, raw_text_to_file_content};
