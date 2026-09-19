pub mod ripgrep;

use std::io;
use std::path::Path;

/// True if `e` reports that an advisory `flock` is held by another process.
/// Unix reports this as `WouldBlock`; Windows as `ERROR_LOCK_VIOLATION` (OS error 33), matched via [`fs2::lock_contended_error`].
pub fn is_lock_contended(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
        || (e.raw_os_error().is_some()
            && e.raw_os_error() == fs2::lock_contended_error().raw_os_error())
}

/// Uses symlink_metadata so a dangling symlink counts, and treats unreadable as present so EACCES cannot hide a gated entry.
pub(crate) fn path_present_or_uncertain(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_lock_contended_classifies_errors() {
        assert!(is_lock_contended(&fs2::lock_contended_error()));
        assert!(is_lock_contended(&io::Error::new(
            io::ErrorKind::WouldBlock,
            "would block"
        )));
        #[cfg(windows)]
        assert!(is_lock_contended(&io::Error::from_raw_os_error(33)));
        assert!(!is_lock_contended(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "denied"
        )));
    }
}
