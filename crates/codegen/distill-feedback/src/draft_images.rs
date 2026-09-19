//! Composer images copied onto one local feedback draft: `feedback_draft_images/<id>/{N.<ext>, metadata.json}`.
//! The manifest is the only index; caps and the mime allow-list are the caller's [`DraftImagePolicy`].

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::FeedbackDraftId;
use crate::draft_store::{
    FEEDBACK_DRAFT_IMAGES_DIRNAME, feedback_draft_images_dir, read_regular_capped,
};

const MANIFEST_FILENAME: &str = "metadata.json";
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct DraftImagePolicy {
    pub max_images: usize,
    pub max_image_bytes: usize,
    /// On-disk extension for an allowed mime type; `None` rejects it.
    pub extension_for_mime: fn(&str) -> Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct DraftImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DraftImageError {
    #[error("feedback draft image {index} violates the image policy: {reason}")]
    Rejected { index: usize, reason: &'static str },
    #[error("write feedback draft images {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("encode feedback draft image manifest: {source}")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
}

/// The on-disk manifest entry; its keys are frozen.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DraftImageManifestEntry {
    file_name: String,
    mime_type: String,
    byte_len: usize,
}

/// Validates every image, then writes `N.<ext>` and the manifest into a freshly created `<id>` directory: `id` is
/// fresh and everything is `create_new`, so a planted path fails the write, and on `Err` nothing this call created survives.
pub fn write_draft_images(
    session_dir: &Path,
    id: &FeedbackDraftId,
    images: &[DraftImage],
    policy: DraftImagePolicy,
) -> Result<(), DraftImageError> {
    if images.is_empty() {
        return Ok(());
    }
    if images.len() > policy.max_images {
        return Err(DraftImageError::Rejected {
            index: policy.max_images,
            reason: "over the image count limit",
        });
    }
    let manifest = images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let reject = |reason| DraftImageError::Rejected { index, reason };
            if image.bytes.is_empty() {
                return Err(reject("empty"));
            }
            if image.bytes.len() > policy.max_image_bytes {
                return Err(reject("over the size limit"));
            }
            let extension = (policy.extension_for_mime)(&image.mime_type)
                .ok_or_else(|| reject("unsupported mime type"))?;
            Ok(DraftImageManifestEntry {
                file_name: format!("{index}.{extension}"),
                mime_type: image.mime_type.clone(),
                byte_len: image.bytes.len(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|source| DraftImageError::Encode { source })?;

    let write_error = |path: &Path, source| DraftImageError::Write {
        path: path.to_path_buf(),
        source,
    };
    let images_root = session_dir.join(FEEDBACK_DRAFT_IMAGES_DIRNAME);
    std::fs::create_dir_all(&images_root).map_err(|source| write_error(&images_root, source))?;
    let dir = feedback_draft_images_dir(session_dir, id);
    std::fs::create_dir(&dir).map_err(|source| write_error(&dir, source))?;
    let write_files = || -> Result<(), DraftImageError> {
        for (entry, image) in manifest.iter().zip(images) {
            let path = dir.join(&entry.file_name);
            write_new(&path, &image.bytes).map_err(|source| write_error(&path, source))?;
        }
        let manifest_path = dir.join(MANIFEST_FILENAME);
        write_new(&manifest_path, &manifest_bytes)
            .map_err(|source| write_error(&manifest_path, source))
    };
    write_files().inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&dir);
    })
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    File::create_new(path)?.write_all(bytes)
}

/// The first `max_images` manifest entries that pass the name, mime, and exact-length checks; no readable manifest, no images.
#[must_use]
pub fn read_draft_images(
    session_dir: &Path,
    id: &FeedbackDraftId,
    policy: DraftImagePolicy,
) -> Vec<DraftImage> {
    let dir = feedback_draft_images_dir(session_dir, id);
    let Some(manifest) = read_regular_capped(&dir.join(MANIFEST_FILENAME), MAX_MANIFEST_BYTES)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<DraftImageManifestEntry>>(&bytes).ok())
    else {
        return Vec::new();
    };
    manifest
        .into_iter()
        .take(policy.max_images)
        .filter_map(|entry| {
            let mut components = Path::new(&entry.file_name).components();
            let has_safe_name = matches!(
                (components.next(), components.next()),
                (Some(Component::Normal(_)), None)
            );
            if !has_safe_name
                || (policy.extension_for_mime)(&entry.mime_type).is_none()
                || !(1..=policy.max_image_bytes).contains(&entry.byte_len)
            {
                return None;
            }
            let bytes = read_regular_capped(&dir.join(&entry.file_name), entry.byte_len).ok()?;
            (bytes.len() == entry.byte_len).then_some(DraftImage {
                bytes,
                mime_type: entry.mime_type,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "draft_images_tests.rs"]
mod tests;
