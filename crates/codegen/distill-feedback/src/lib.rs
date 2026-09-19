//! Shared feedback taxonomy, structured metadata helpers, durable local draft storage, and the capped session trace archive.

#![deny(clippy::indexing_slicing)]

mod draft_images;
mod draft_store;
mod feedback_archive;
mod taxonomy;
mod text;

pub use draft_images::{
    DraftImage, DraftImageError, DraftImagePolicy, read_draft_images, write_draft_images,
};
pub use draft_store::{
    DeleteOutcome, FEEDBACK_DRAFT_IMAGES_DIRNAME, FEEDBACK_DRAFTS_FILENAME,
    FEEDBACK_DRAFTS_LOCK_FILENAME, FEEDBACK_DRAFTS_TEMP_PREFIX, FeedbackDraft,
    FeedbackDraftArtifactSet, FeedbackDraftInput, FeedbackDraftStore, FeedbackStoreError, Result,
    UpdateOutcome, feedback_draft_images_dir, is_feedback_draft_artifact_name,
    open_regular_nofollow, read_regular_capped, validate_feedback_draft_send,
};
pub use feedback_archive::{
    ArchiveCaps, ArchiveError, FEEDBACK_ARCHIVE_CAPS, build_session_archive,
};
pub use taxonomy::{
    FeedbackDraftId, FeedbackFailureMode, FeedbackSource, FeedbackTaskCategory, FeedbackTaxonomy,
    FeedbackType, StructuredFeedback, parse_structured_feedback, structured_feedback,
    taxonomy_prompt, wire_value,
};
pub use text::{derive_title, post_text};
