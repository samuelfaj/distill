// Modified for Distill by Samuel Fajreldines, 2026.
//! Feedback image policy shared by the send paths ([`select_feedback_images`], so a dropped image never drops the report) and the draft-image readers for the modal's Drafts tab.

use std::path::Path;

use distill_feedback::{DraftImagePolicy, FeedbackDraftId, read_draft_images};
use distill_shell::session::{
    MAX_FEEDBACK_IMAGE_BYTES, MAX_FEEDBACK_IMAGE_TOTAL_BYTES, MAX_FEEDBACK_IMAGES,
    feedback_image_extension,
};

/// Applies the send-time image policy in order; `None` is an image whose bytes could not be read or decoded.
/// Returns the accepted indices plus the user notice for anything dropped.
pub(crate) fn select_feedback_images(
    images: &[Option<(Vec<u8>, String)>],
) -> (Vec<usize>, Option<String>) {
    let mut accepted = Vec::new();
    let mut over_count = 0usize;
    let mut unsupported = 0usize;
    let mut too_large = 0usize;
    let mut unreadable = 0usize;
    let mut total_bytes = 0usize;
    for (index, image) in images.iter().enumerate() {
        let Some((bytes, mime_type)) = image.as_ref().filter(|(bytes, _)| !bytes.is_empty()) else {
            unreadable += 1;
            continue;
        };
        if accepted.len() >= MAX_FEEDBACK_IMAGES {
            over_count += 1;
            continue;
        }
        if feedback_image_extension(mime_type).is_none() {
            unsupported += 1;
            continue;
        }
        if bytes.len() > MAX_FEEDBACK_IMAGE_BYTES
            || total_bytes + bytes.len() > MAX_FEEDBACK_IMAGE_TOTAL_BYTES
        {
            too_large += 1;
            continue;
        }
        total_bytes += bytes.len();
        accepted.push(index);
    }
    let dropped = over_count + unsupported + too_large + unreadable;
    let notice = (dropped > 0).then(|| {
        const MIB: usize = 1024 * 1024;
        let mut reasons = Vec::new();
        if over_count > 0 {
            reasons.push(format!(
                "{over_count} over the {MAX_FEEDBACK_IMAGES}-image limit"
            ));
        }
        if unsupported > 0 {
            reasons.push(format!(
                "{unsupported} in a format feedback can't carry (PNG, JPEG, or GIF only)"
            ));
        }
        if too_large > 0 {
            reasons.push(format!(
                "{too_large} over the size limit ({} MB each, {} MB combined)",
                MAX_FEEDBACK_IMAGE_BYTES / MIB,
                MAX_FEEDBACK_IMAGE_TOTAL_BYTES / MIB,
            ));
        }
        if unreadable > 0 {
            reasons.push(format!("{unreadable} unreadable"));
        }
        let plural = if dropped == 1 { "" } else { "s" };
        format!(
            "Dropped {dropped} image{plural} from the feedback: {}.",
            reasons.join(", ")
        )
    });
    (accepted, notice)
}

const DRAFT_IMAGE_POLICY: DraftImagePolicy = DraftImagePolicy {
    max_images: MAX_FEEDBACK_IMAGES,
    max_image_bytes: MAX_FEEDBACK_IMAGE_BYTES,
    extension_for_mime: feedback_image_extension,
};

pub(super) fn attach_saved_draft_images(
    modal: &mut crate::views::feedback_modal::FeedbackModalState,
    session_dir: &Path,
    draft_id: &FeedbackDraftId,
) {
    for image in read_feedback_draft_images(session_dir, draft_id) {
        let _ = modal.insert_image(image);
    }
}

fn read_feedback_draft_images(
    session_dir: &Path,
    draft_id: &FeedbackDraftId,
) -> Vec<crate::prompt_images::PastedImage> {
    read_draft_images(session_dir, draft_id, DRAFT_IMAGE_POLICY)
        .into_iter()
        .map(|image| {
            crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
                data: image.bytes,
                mime_type: image.mime_type,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(len: usize) -> Option<(Vec<u8>, String)> {
        Some((vec![0u8; len], "image/png".to_owned()))
    }

    /// `(case name, decoded images, expected accepted indices, expected notice)`.
    type Case = (
        &'static str,
        Vec<Option<(Vec<u8>, String)>>,
        Vec<usize>,
        Option<&'static str>,
    );

    /// Each policy branch drops its image and names the reason; the report itself is never refused.
    #[test]
    fn select_feedback_images_applies_each_policy_branch_in_order() {
        let cases: [Case; 6] = [
            ("all good", vec![png(16), png(16)], vec![0, 1], None),
            (
                "over count",
                vec![png(16); MAX_FEEDBACK_IMAGES + 1],
                (0..MAX_FEEDBACK_IMAGES).collect(),
                Some("Dropped 1 image from the feedback: 1 over the 4-image limit."),
            ),
            (
                "unsupported mime",
                vec![Some((vec![1], "image/webp".to_owned())), png(16)],
                vec![1],
                Some(
                    "Dropped 1 image from the feedback: 1 in a format feedback can't carry (PNG, JPEG, or GIF only).",
                ),
            ),
            (
                "too large",
                vec![png(MAX_FEEDBACK_IMAGE_BYTES + 1)],
                vec![],
                Some(
                    "Dropped 1 image from the feedback: 1 over the size limit (8 MB each, 16 MB combined).",
                ),
            ),
            (
                "unreadable and empty",
                vec![None, Some((vec![], "image/png".to_owned())), png(16)],
                vec![2],
                Some("Dropped 2 images from the feedback: 2 unreadable."),
            ),
            (
                "combined cap counts as too large",
                vec![
                    png(MAX_FEEDBACK_IMAGE_BYTES),
                    png(MAX_FEEDBACK_IMAGE_BYTES),
                    png(1),
                ],
                vec![0, 1],
                Some(
                    "Dropped 1 image from the feedback: 1 over the size limit (8 MB each, 16 MB combined).",
                ),
            ),
        ];
        for (name, images, expected_accepted, expected_notice) in cases {
            let (accepted, notice) = select_feedback_images(&images);
            assert_eq!(expected_accepted, accepted, "{name}");
            assert_eq!(expected_notice.map(str::to_owned), notice, "{name}");
        }
    }
}
