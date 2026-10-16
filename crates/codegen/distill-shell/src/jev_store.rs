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
    std::fs::create_dir_all(dir).ok()?;
    if std::fs::symlink_metadata(&path).is_ok() {
        let resolved = std::fs::canonicalize(&path).ok()?;
        if resolved.parent()? != std::fs::canonicalize(dir).ok()? || std::fs::read(&resolved).ok()? != payload.as_bytes() {
            return None;
        }
        return Some(path);
    }
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new_in(dir).ok()?;
    file.write_all(payload.as_bytes()).ok()?;
    match file.persist_noclobber(&path) {
        Ok(_) => Some(path),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let resolved = std::fs::canonicalize(&path).ok()?;
            (resolved.parent()? == std::fs::canonicalize(dir).ok()?
                && std::fs::read(&resolved).ok()? == payload.as_bytes()).then_some(path)
        },
        Err(_) => None,
    }
}

/// Session membership and provenance only; original bytes stay in the existing store.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionOutput {
    pub session_id: String,
    pub tool: String,
    pub call_id: String,
    pub is_error: bool,
    #[serde(default)]
    pub status: Option<String>,
    pub path: PathBuf,
}

pub(crate) const SEARCH_CHUNK_BYTES: usize = 2048;
pub(crate) const MAX_ORIGINAL_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_SOURCE_CHUNKS: usize = 512;
pub(crate) const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_MANIFEST_ENTRIES: usize = 256;
pub(crate) const MAX_SESSION_CHUNKS: usize = 4096;
const MAX_ENTRY_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct OutputChunk {
    pub start: usize,
    pub end: usize,
    pub offset: u64,
    pub bytes: usize,
    pub sha256: String,
    pub terms: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct IndexedOutput {
    #[serde(flatten)]
    pub source: SessionOutput,
    pub chunks: Vec<OutputChunk>,
    pub skipped_lines: usize,
    pub unavailable: bool,
    #[serde(default)]
    pub limit_exceeded: bool,
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn lexical_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|term| !term.is_empty()).map(str::to_lowercase)
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect()
}

/// Index terms and byte ranges, not duplicate excerpt text. Secrets stay unindexed.
pub(crate) fn index_output(source: SessionOutput, text: &str) -> IndexedOutput {
    let secret = [&source.session_id, &source.tool, &source.call_id, source.status.as_deref().unwrap_or(""), text]
        .iter().any(|part| distill_workspace::jev::crushers::utility_secret_presence(part).is_some());
    let mut indexed = IndexedOutput { source, chunks: Vec::new(), skipped_lines: 0, limit_exceeded: text.len() > MAX_ORIGINAL_BYTES, unavailable: secret || text.len() > MAX_ORIGINAL_BYTES };
    if indexed.unavailable { return indexed; }
    let mut offset = 0;
    let mut start_offset = 0;
    let mut start = 1;
    let mut number = 0;
    let push = |chunks: &mut Vec<OutputChunk>, from: usize, to: usize, start: usize, end: usize| {
        if to <= from { return; }
        let body = &text[from..to];
        chunks.push(OutputChunk { start, end, offset: from as u64, bytes: to - from,
            sha256: sha256(body.as_bytes()), terms: lexical_terms(body) });
    };
    for line in text.split_inclusive('\n') {
        number += 1;
        if offset - start_offset + line.len() > SEARCH_CHUNK_BYTES || number - start >= 12 {
            push(&mut indexed.chunks, start_offset, offset, start, number - 1);
            if indexed.chunks.len() >= MAX_SOURCE_CHUNKS {
                indexed.chunks.clear(); indexed.unavailable = true; indexed.limit_exceeded = true;
                return indexed;
            }
            start_offset = offset;
            start = number;
        }
        offset += line.len();
        if line.len() > SEARCH_CHUNK_BYTES {
            indexed.skipped_lines += 1;
            start_offset = offset;
            start = number + 1;
        }
    }
    push(&mut indexed.chunks, start_offset, offset, start, number);
    if indexed.chunks.len() > MAX_SOURCE_CHUNKS {
        indexed.chunks.clear(); indexed.unavailable = true; indexed.limit_exceeded = true;
    }
    indexed
}

pub(crate) fn manifest_path(root: &Path, session_id: &str) -> PathBuf {
    root.join("output-manifests").join(format!("{}.jsonl", sha256(session_id.as_bytes())))
}

pub(crate) fn session_manifest(session_id: &str) -> PathBuf {
    manifest_path(&crate::util::distill_home::distill_home().join("jev"), session_id)
}

fn open_manifest(manifest: &Path, append: bool) -> std::io::Result<std::fs::File> {
    let parent = manifest.parent().ok_or_else(|| std::io::Error::other("manifest has no parent"))?;
    if append { std::fs::create_dir_all(parent)?; }
    if std::fs::symlink_metadata(parent)?.file_type().is_symlink() {
        return Err(std::io::Error::other("output manifest directory is a symlink"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(!append).append(append).create(append);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        if append { options.mode(0o600); }
    }
    #[cfg(not(unix))]
    if manifest.symlink_metadata().is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(std::io::Error::other("output manifest is a symlink"));
    }
    options.open(manifest)
}

/// Append-only sidecar: never reads or rewrites conversation history.
pub(crate) fn register_session_output(manifest: &Path, entry: &SessionOutput) -> std::io::Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(&entry.path)?.take(MAX_ORIGINAL_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ORIGINAL_BYTES { return Err(std::io::Error::other("session output original byte limit exceeded")); }
    let text = String::from_utf8(bytes).map_err(std::io::Error::other)?;
    register_indexed_output(manifest, &index_output(entry.clone(), &text))
}

fn validate_entry(entry: &IndexedOutput) -> std::io::Result<()> {
    if entry.limit_exceeded || entry.chunks.len() > MAX_SOURCE_CHUNKS {
        return Err(std::io::Error::other("session output source chunk/byte limit exceeded"));
    }
    Ok(())
}

pub(crate) fn register_indexed_output(manifest: &Path, entry: &IndexedOutput) -> std::io::Result<()> {
    use std::io::Write;
    // Ownership survives optional index rejection, including shared originals.
    if !entry.source.path.as_os_str().is_empty() {
        mark_output_member(&entry.source.path, &entry.source.session_id)?;
    }
    validate_entry(entry)?;
    static APPEND: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = APPEND.lock().map_err(|_| std::io::Error::other("output manifest lock poisoned"))?;
    let mut bytes = serde_json::to_vec(entry)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_ENTRY_BYTES { return Err(std::io::Error::other("session output entry byte limit exceeded")); }
    let prior = session_outputs(manifest, &entry.source.session_id)?;
    if prior.len() >= MAX_MANIFEST_ENTRIES { return Err(std::io::Error::other("session output manifest entry limit exceeded")); }
    if prior.iter().map(|entry| entry.chunks.len()).sum::<usize>() + entry.chunks.len() > MAX_SESSION_CHUNKS {
        return Err(std::io::Error::other("session output total chunk limit exceeded"));
    }
    let mut file = open_manifest(manifest, true)?;
    if file.metadata()?.len() + bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(std::io::Error::other("session output manifest byte limit exceeded"));
    }
    file.write_all(&bytes)
}

pub(crate) fn session_outputs(manifest: &Path, session_id: &str) -> std::io::Result<Vec<IndexedOutput>> {
    use std::io::{BufRead, Read};
    let file = match open_manifest(manifest, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > MAX_MANIFEST_BYTES { return Err(std::io::Error::other("session output manifest byte limit exceeded")); }
    let mut reader = std::io::BufReader::new(file).take(MAX_MANIFEST_BYTES + 1);
    let mut entries = Vec::new();
    let mut total_chunks = 0;
    loop {
        let mut line = Vec::new();
        let read = reader.by_ref().take(MAX_ENTRY_BYTES + 1).read_until(b'\n', &mut line)?;
        if read == 0 { break; }
        if read as u64 > MAX_ENTRY_BYTES || reader.limit() == 0 {
            return Err(std::io::Error::other("session output manifest/entry byte limit exceeded"));
        }
        if entries.len() >= MAX_MANIFEST_ENTRIES { return Err(std::io::Error::other("session output manifest entry limit exceeded")); }
        let entry: IndexedOutput = serde_json::from_slice(&line)?;
        validate_entry(&entry)?;
        total_chunks += entry.chunks.len();
        if total_chunks > MAX_SESSION_CHUNKS { return Err(std::io::Error::other("session output total chunk limit exceeded")); }
        if entry.source.session_id != session_id { return Err(std::io::Error::other("output manifest session mismatch")); }
        entries.push(entry);
    }
    Ok(entries)
}

fn ownership_dir(path: &Path) -> std::io::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    Ok(path.parent().ok_or_else(|| std::io::Error::other("original has no parent"))?
        .join("output-membership").join(sha256(path.as_os_str().as_encoded_bytes())))
}

fn reject_symlink(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(std::io::Error::other("output membership symlink refused"));
    }
    Ok(())
}

fn mark_output_member(path: &Path, session: &str) -> std::io::Result<()> {
    let directory = ownership_dir(path)?;
    let parent = directory.parent().ok_or_else(|| std::io::Error::other("membership has no parent"))?;
    std::fs::create_dir_all(parent)?;
    reject_symlink(parent)?;
    std::fs::create_dir_all(&directory)?;
    reject_symlink(&directory)?;
    let marker = directory.join(sha256(session.as_bytes()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NOFOLLOW).mode(0o600); }
    #[cfg(not(unix))] if marker.exists() { reject_symlink(&marker)?; }
    options.open(marker)?;
    Ok(())
}

/// Legacy originals have no membership metadata; newly indexed originals require membership.
pub(crate) fn output_member_or_legacy(path: &Path, session: &str) -> std::io::Result<bool> {
    let directory = ownership_dir(path)?;
    let parent = directory.parent().ok_or_else(|| std::io::Error::other("membership has no parent"))?;
    match std::fs::symlink_metadata(parent) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error), Ok(_) => reject_symlink(parent)?,
    }
    match std::fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error), Ok(_) => reject_symlink(&directory)?,
    }
    let marker = directory.join(sha256(session.as_bytes()));
    match std::fs::symlink_metadata(&marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error), Ok(_) => { reject_symlink(&marker)?; Ok(marker.is_file()) },
    }
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
    #[test]
    fn session_output_manifest_isolates_membership_and_fails_on_corruption() {
        let root = tempfile::tempdir().expect("root");
        let first = manifest_path(root.path(), "session/one");
        let second = manifest_path(root.path(), "session/two");
        let entry = SessionOutput {
            session_id: "session/one".to_owned(), tool: "grep".to_owned(),
            call_id: "call-1".to_owned(), is_error: true, status: Some("exit: 2".to_owned()),
            path: root.path().join("original.txt"),
        };
        std::fs::write(&entry.path, "original\n").expect("original");
        register_session_output(&first, &entry).expect("register");
        assert_eq!(session_outputs(&first, "session/one").expect("members")[0].source.call_id, "call-1");
        assert!(session_outputs(&second, "session/two").expect("empty session").is_empty());
        assert!(session_outputs(&first, "session/two").is_err(), "copied or colliding manifests cannot cross sessions");
        std::fs::write(&first, "{broken JSON").expect("corrupt");
        assert!(session_outputs(&first, "session/one").is_err(), "never report corrupt manifest as no matches");
    }

    #[cfg(unix)]
    #[test]
    fn session_output_manifest_rejects_symlink_file_and_directory() {
        let root = tempfile::tempdir().expect("root");
        let manifest = manifest_path(root.path(), "one");
        std::fs::create_dir_all(manifest.parent().expect("parent")).expect("parent");
        let outside = root.path().join("outside");
        std::fs::write(&outside, "").expect("outside");
        std::os::unix::fs::symlink(&outside, &manifest).expect("link");
        assert!(session_outputs(&manifest, "one").is_err());
        std::fs::remove_file(&manifest).expect("unlink");
        std::fs::remove_dir(manifest.parent().expect("parent")).expect("remove parent");
        std::os::unix::fs::symlink(root.path(), manifest.parent().expect("parent")).expect("parent symlink");
        assert!(session_outputs(&manifest, "one").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn session_output_store_rejects_outside_and_broken_symlinks() {
        let root = tempfile::tempdir().expect("root");
        let store = root.path().join("store");
        std::fs::create_dir(&store).expect("store");
        let payload = "needle data";
        let path = store.join(format!("{}.txt", distill_workspace::jev::reduce::content_hash(payload)));
        let outside = root.path().join("outside");
        std::fs::write(&outside, payload).expect("outside");
        std::os::unix::fs::symlink(&outside, &path).expect("link");
        assert!(store_payload_in(&store, payload).is_none());
        std::fs::remove_file(&outside).expect("remove outside");
        assert!(store_payload_in(&store, payload).is_none(), "broken symlink cannot cause retries or writes outside store");
    }

    #[test]
    fn session_output_limits_fail_visibly_before_unbounded_chunk_or_json_allocation() {
        use std::io::Write;
        let root = tempfile::tempdir().expect("root");
        let path = store_payload_in(root.path(), "original").expect("original");
        let source = SessionOutput { session_id: "one".to_owned(), tool: "test".to_owned(), call_id: "one".to_owned(),
            is_error: false, status: None, path };
        let indexed = index_output(source.clone(), &"\n".repeat(MAX_ORIGINAL_BYTES));
        assert!(indexed.limit_exceeded);
        assert!(indexed.chunks.is_empty(), "no partial semantic index");
        let manifest = manifest_path(root.path(), "one");
        assert!(register_indexed_output(&manifest, &indexed).unwrap_err().to_string().contains("limit"));
        std::fs::create_dir_all(manifest.parent().expect("parent")).expect("parent");
        let mut file = std::fs::File::create(&manifest).expect("manifest");
        file.write_all(&vec![b' '; MAX_ENTRY_BYTES as usize + 1]).expect("oversized line");
        assert!(session_outputs(&manifest, "one").unwrap_err().to_string().contains("byte limit"));
        file.set_len(MAX_MANIFEST_BYTES + 1).expect("sparse oversized manifest");
        assert!(session_outputs(&manifest, "one").unwrap_err().to_string().contains("byte limit"));
        let mut entry = index_output(source, "original");
        entry.chunks.clear();
        std::fs::write(&manifest, "").expect("reset");
        let mut bytes = serde_json::to_vec(&entry).expect("serialize"); bytes.push(b'\n');
        std::fs::write(&manifest, bytes.repeat(MAX_MANIFEST_ENTRIES + 1)).expect("too many entries");
        assert!(session_outputs(&manifest, "one").unwrap_err().to_string().contains("entry limit"));
    }

    #[test]
    fn session_output_rejected_index_keeps_unique_and_shared_ownership() {
        let root = tempfile::tempdir().expect("root");
        let source = |session: &str, path: PathBuf| SessionOutput {
            session_id: session.to_owned(), tool: "test".to_owned(), call_id: "call".to_owned(),
            is_error: false, status: None, path,
        };
        let manifest = manifest_path(root.path(), "A");
        std::fs::create_dir_all(manifest.parent().expect("parent")).expect("parent");
        let filler = index_output(source("A", PathBuf::new()), "");
        let mut bytes = serde_json::to_vec(&filler).expect("serialize"); bytes.push(b'\n');
        std::fs::write(&manifest, bytes.repeat(MAX_MANIFEST_ENTRIES)).expect("full manifest");
        let unique = store_payload_in(root.path(), "unique A output").expect("unique");
        let error = register_indexed_output(&manifest, &index_output(source("A", unique.clone()), "unique A output"))
            .expect_err("entry limit");
        assert!(error.to_string().contains("entry limit"));
        assert!(output_member_or_legacy(&unique, "A").expect("owner"));
        assert!(!output_member_or_legacy(&unique, "B").expect("other session denied"));
        let shared = store_payload_in(root.path(), "shared output").expect("shared");
        register_indexed_output(&manifest_path(root.path(), "B"),
            &index_output(source("B", shared.clone()), "shared output")).expect("B owns shared");
        assert!(register_indexed_output(&manifest, &index_output(source("A", shared.clone()), "shared output")).is_err());
        assert!(output_member_or_legacy(&shared, "A").expect("rejected index owner recoverable"));
        assert!(output_member_or_legacy(&shared, "B").expect("prior owner recoverable"));
        assert!(!output_member_or_legacy(&shared, "C").expect("third session denied"));
        assert_eq!(std::fs::read_to_string(&shared).expect("shared preserved"), "shared output");
    }

    #[cfg(unix)]
    #[test]
    fn session_output_ownership_failure_prevents_index_admission_without_deleting_shared_original() {
        let root = tempfile::tempdir().expect("root");
        let path = store_payload_in(root.path(), "shared").expect("shared");
        let directory = ownership_dir(&path).expect("membership");
        std::fs::create_dir_all(directory.parent().expect("parent")).expect("parent");
        std::os::unix::fs::symlink(root.path(), &directory).expect("unsafe membership");
        let manifest = manifest_path(root.path(), "A");
        let entry = SessionOutput { session_id: "A".to_owned(), tool: "test".to_owned(), call_id: "call".to_owned(),
            is_error: false, status: None, path: path.clone() };
        assert!(register_indexed_output(&manifest, &index_output(entry, "shared")).is_err());
        assert!(!manifest.exists(), "ownership failure blocks optional index");
        assert!(output_member_or_legacy(&path, "B").is_err(), "unsafe ownership fails closed");
        assert_eq!(std::fs::read_to_string(path).expect("original retained"), "shared");
    }

}
