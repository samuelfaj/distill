// Modified for Distill by Samuel Fajreldines, 2026.
//! Store-before-loss: the way back to a payload the harness compressed.
//!
//! A lossy reduction is only allowed to run after the original has been written
//! here, and the marker that replaces the text names the file. The model reads it
//! back with its ordinary `read_file`, so the harness needs no new tool and no
//! new protocol for recovery — and the file is content-addressed, so storing the
//! same payload twice is one file.

use std::path::{Path, PathBuf};

/// Directory holding the payloads, under the harness home.
pub fn store_dir() -> PathBuf {
    crate::util::distill_home::distill_home()
        .join("jev")
        .join("store")
}

/// Writes `payload` and returns the path that reads it back byte-for-byte.
///
/// Returns `None` when the bytes cannot be stored: a caller must then keep the
/// original text, because losing it is not an option this module can offer.
pub fn store_payload(payload: &str) -> Option<PathBuf> {
    store_payload_in(&store_dir(), payload)
}

/// Same, against an explicit directory (hermetic tests, and any future caller
/// that wants the store somewhere else).
pub fn store_payload_in(dir: &Path, payload: &str) -> Option<PathBuf> {
    let hash = distill_workspace::jev::reduce::content_hash(payload);
    let path = dir.join(format!("{hash}.txt"));
    if path.is_file() {
        return Some(path);
    }
    std::fs::create_dir_all(dir).ok()?;
    std::fs::write(&path, payload).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the lane: what was compressed is still readable,
    /// byte for byte, through the harness's own file path.
    #[test]
    fn a_stored_payload_reads_back_byte_identical() {
        let dir = tempfile::tempdir().expect("temp dir");
        let payload = "error[E0308]: mismatched types\n  --> src/client.rs:868\n\n\0tab\ttab\n";
        let path = store_payload_in(dir.path(), payload).expect("stores");
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), payload);

        // Content-addressed: the same bytes are one file, and a different
        // payload does not collide with it.
        let again = store_payload_in(dir.path(), payload).expect("stores");
        assert_eq!(again, path);
        let other = store_payload_in(dir.path(), "different bytes").expect("stores");
        assert_ne!(other, path);
    }

    /// An unwritable store must fail the caller into keeping the original, not
    /// into a silent loss: the function says so with `None`.
    #[test]
    fn an_unwritable_store_reports_failure_instead_of_losing_the_payload() {
        let dir = tempfile::tempdir().expect("temp dir");
        let bogus = dir.path().join("a-file");
        std::fs::write(&bogus, "not a directory").expect("write");
        assert!(store_payload_in(&bogus, "payload").is_none());
    }
}
