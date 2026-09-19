// Modified for Distill by Samuel Fajreldines, 2026.
//! `DistillHashline` namespace — hashline-anchored read/edit/search tools.
//!
//! This module provides the anchor engine used by the hashline toolset:
//! - [`AnchorScheme`] trait and implementations (Candidates A, B, C)
//! - Anchor parsing, rendering, and validation
//! - Bounded recovery helpers for shifted/stale anchors
//!
//! The hashline tools themselves (`hashline_read`, `hashline_edit`,
//! `hashline_grep`) build on the reusable core this module provides (also
//! used by the benchmark harness).

pub mod anchor;
pub mod benchmark;
pub mod config;
pub mod edit;
pub mod grep;
pub mod mutate;
pub mod read_file;
pub mod scheme;

pub use config::HashlineSchemeParams;
pub use edit::HashlineEditTool;
pub use grep::HashlineGrepTool;
pub use read_file::HashlineReadTool;

/// In-crate stand-in for the memory crate's v2 access policy.
///
/// The real policy lives in `distill-memory`, which depends on this crate, so
/// tool tests here cannot use it. This fake mirrors the observable contract the
/// tools rely on: a single scope root, protected `MEMORY.md`, writable
/// direct children of `topics/` and `observations/_inbox/`, read-before-edit
/// snapshots, and a manifest refresh after every persisted write.
#[cfg(test)]
pub(crate) mod memory_v2_test_support {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use crate::types::memory_v2::{MemoryV2Access, MemoryV2Write};

    #[derive(Debug)]
    pub(crate) struct FakeMemoryV2Access {
        root: PathBuf,
        snapshots: parking_lot::Mutex<HashMap<PathBuf, Vec<u8>>>,
        recorded_reads: parking_lot::Mutex<Vec<PathBuf>>,
        policy_writes: parking_lot::Mutex<Vec<PathBuf>>,
    }

    impl FakeMemoryV2Access {
        /// Initialize a scope under `root` with an empty manifest and topic dir.
        pub(crate) fn new(root: &Path) -> Self {
            std::fs::create_dir_all(root.join("topics")).unwrap();
            std::fs::create_dir_all(root.join("observations/_inbox")).unwrap();
            std::fs::write(root.join("MEMORY.md"), "# Memory\n").unwrap();
            std::fs::write(root.join("memory_state.sqlite"), b"sqlite").unwrap();
            Self {
                root: root.to_path_buf(),
                snapshots: parking_lot::Mutex::new(HashMap::new()),
                recorded_reads: parking_lot::Mutex::new(Vec::new()),
                policy_writes: parking_lot::Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn recorded_reads(&self) -> Vec<PathBuf> {
            self.recorded_reads.lock().clone()
        }

        pub(crate) fn policy_writes(&self) -> Vec<PathBuf> {
            self.policy_writes.lock().clone()
        }

        fn relative(&self, path: &Path) -> Option<PathBuf> {
            path.strip_prefix(&self.root).ok().map(Path::to_path_buf)
        }

        fn refresh_manifest(&self) {
            let mut manifest = String::from("# Memory\n\n## Topics\n");
            let mut topics: Vec<PathBuf> = std::fs::read_dir(self.root.join("topics"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            topics.sort();
            for topic in topics {
                let name = topic.file_name().unwrap().to_string_lossy();
                manifest.push_str(&format!("- topics/{name}\n"));
            }
            std::fs::write(self.root.join("MEMORY.md"), manifest).unwrap();
        }
    }

    impl MemoryV2Access for FakeMemoryV2Access {
        fn validate_read(&self, path: &Path) -> Result<bool, String> {
            Ok(self.relative(path).is_some())
        }

        fn record_read(&self, path: &Path, contents: &[u8]) -> Result<(), String> {
            if self.relative(path).is_some() {
                self.snapshots
                    .lock()
                    .insert(path.to_path_buf(), contents.to_vec());
                self.recorded_reads.lock().push(path.to_path_buf());
            }
            Ok(())
        }

        fn preflight_write(&self, path: &Path, _contents: &[u8]) -> Result<bool, String> {
            let Some(relative) = self.relative(path) else {
                return Ok(false);
            };
            let writable = relative.parent() == Some(Path::new("topics"))
                || relative.parent() == Some(Path::new("observations/_inbox"));
            if !writable {
                return Err(format!(
                    "writes are not allowed to protected memory v2 path: {}",
                    path.display()
                ));
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
                return Err(format!(
                    "memory v2 writes require a .md file: {}",
                    path.display()
                ));
            }
            if let Ok(previous) = std::fs::read(path) {
                match self.snapshots.lock().get(path) {
                    None => return Err(format!("read {} before editing it", path.display())),
                    Some(snapshot) if snapshot != &previous => {
                        return Err(format!(
                            "memory v2 file changed since it was read; read it again before editing: {}",
                            path.display()
                        ));
                    }
                    Some(_) => {}
                }
            }
            Ok(true)
        }

        fn write_file(&self, path: &Path, contents: &[u8]) -> Result<MemoryV2Write, String> {
            if !self.preflight_write(path, contents)? {
                return Ok(MemoryV2Write::Outside);
            }
            let previous_content = std::fs::read(path).ok();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::write(path, contents).map_err(|error| error.to_string())?;
            self.refresh_manifest();
            self.snapshots
                .lock()
                .insert(path.to_path_buf(), contents.to_vec());
            self.policy_writes.lock().push(path.to_path_buf());
            Ok(MemoryV2Write::Written { previous_content })
        }

        fn scope_roots(&self) -> [PathBuf; 2] {
            [self.root.clone(), self.root.clone()]
        }
    }
}
