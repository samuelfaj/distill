// Modified for Distill by Samuel Fajreldines, 2026.
//! Markdown-based memory file storage.
//!
//! Handles reading and writing memory files (`.md`) for both global and workspace-scoped memory.
//! All workspace-scoped memory lives under `~/.grok/memory/{project-slug}-{hash8}/` to avoid polluting the user's repo.

use std::path::{Path, PathBuf};

use distill_config_types::MemoryMode;
use distill_tools::util::distill_home::distill_home;

/// Write-operation scope. Distinct from `distill_agent::config::MemoryScope` (agent memory dir).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    /// Global memory, shared across all workspaces.
    Global,
    /// Workspace-scoped memory, specific to one project.
    Workspace,
}

#[derive(Debug, thiserror::Error)]
pub enum SaveRememberNoteError {
    #[error(transparent)]
    Legacy(#[from] std::io::Error),
    #[error(transparent)]
    V2(#[from] crate::v2::V2StorageError),
}

/// Handles file I/O for the memory storage layer.
/// Memory files are human-readable/editable Markdown stored under `~/.grok/memory/`.
/// Workspace-scoped files live under a directory named `{project-slug}-{hash8}`, e.g. `~/.grok/memory/xai-a3f7b2c9/`.
#[derive(Debug, Clone)]
pub struct MemoryStorage {
    mode: MemoryMode,
    /// `~/.grok/memory/`
    global_dir: PathBuf,
    /// `~/.grok/memory/{project-slug}-{hash8}/`
    workspace_dir: PathBuf,
    /// The original workspace path (for logging / diagnostics).
    workspace_path: PathBuf,
    /// When true, workspace writes are silently skipped (temp-dir CWDs).
    ephemeral: bool,
}

impl MemoryStorage {
    /// Create a new `MemoryStorage` rooted at `~/.grok/memory/`.
    /// The workspace directory name is `{slug}-{hash8}` where `slug` is the project directory name and `hash8` is 8 hex chars from blake3.
    /// Directories are created lazily on first write, not here.
    pub fn new(cwd: &Path, root_override: Option<&Path>) -> Self {
        Self::new_inner(cwd, root_override, true)
    }

    /// Create storage for the resolved session mode.
    ///
    /// V2 uses an isolated root and cannot observe files under the legacy root.
    pub fn new_for_mode(cwd: &Path, root_override: Option<&Path>, mode: MemoryMode) -> Self {
        match mode {
            MemoryMode::Legacy => Self::new(cwd, root_override),
            MemoryMode::V2 => {
                let root = root_override
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| distill_home().join("memory-v2"));
                Self {
                    mode,
                    global_dir: root.join("global"),
                    workspace_dir: root.join("workspaces").join(compute_workspace_hash(cwd)),
                    workspace_path: cwd.to_path_buf(),
                    ephemeral: is_ephemeral_cwd(cwd),
                }
            }
        }
    }

    /// Create a MemoryStorage with a flat root (no workspace hash subdirectory).
    /// Used for project/local-scoped agent memory where the root is already project-specific.
    pub fn new_flat(cwd: &Path, root: &Path) -> Self {
        Self::new_inner(cwd, Some(root), false)
    }

    fn new_inner(cwd: &Path, root_override: Option<&Path>, use_workspace_hash: bool) -> Self {
        let global_dir = root_override
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| distill_home().join("memory"));
        let workspace_dir = if use_workspace_hash {
            let workspace_hash = compute_workspace_hash(cwd);
            global_dir.join(&workspace_hash)
        } else {
            global_dir.clone()
        };

        let ephemeral = use_workspace_hash && is_ephemeral_cwd(cwd);

        Self {
            mode: MemoryMode::Legacy,
            global_dir,
            workspace_dir,
            workspace_path: cwd.to_path_buf(),
            ephemeral,
        }
    }

    /// Create a `MemoryStorage` with explicit paths (for testing).
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_paths(global_dir: PathBuf, workspace_dir: PathBuf) -> Self {
        Self {
            mode: MemoryMode::Legacy,
            global_dir,
            workspace_dir,
            workspace_path: PathBuf::from("/test/workspace"),
            ephemeral: false,
        }
    }

    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    pub fn mode(&self) -> MemoryMode {
        self.mode
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    /// Returns the original workspace path.
    pub fn workspace_path(&self) -> &Path {
        &self.workspace_path
    }

    /// Returns `true` if this storage targets an ephemeral (temp-dir) workspace.
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// Count total indexed chunks via a read-only SQLite connection.
    /// Returns 0 if the index doesn't exist or the query fails.
    pub fn total_chunk_count(&self) -> usize {
        let db_path = self.workspace_dir.join("index.sqlite");
        // Journal-mode-aware open: never mmap a legacy WAL -shm on network mounts (SIGBUS); see distill_sqlite_journal::JournalMode::open_readonly
        distill_sqlite_journal::JournalMode::for_db_path(&db_path)
            .open_readonly(&db_path)
            .and_then(|c| c.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get::<_, i64>(0)))
            .unwrap_or(0) as usize
    }

    /// Path to the global `MEMORY.md`.
    pub fn global_memory_file(&self) -> PathBuf {
        self.global_dir.join("MEMORY.md")
    }

    /// Path to the workspace-scoped `MEMORY.md`.
    pub fn workspace_memory_file(&self) -> PathBuf {
        self.workspace_dir.join("MEMORY.md")
    }

    /// Classify a file path as a memory source type.
    ///
    /// Returns `"global"`, `"workspace"`, or `"session"` based on location.
    pub fn classify_source(&self, path: &Path) -> &'static str {
        if self.mode.is_v2() {
            return if path.starts_with(&self.workspace_dir) {
                "workspace"
            } else if path.starts_with(&self.global_dir) {
                "global"
            } else {
                "session"
            };
        }
        if path.starts_with(&self.workspace_dir) {
            if self.mode == MemoryMode::V2 || path.file_name().is_some_and(|f| f == "MEMORY.md") {
                "workspace"
            } else {
                "session"
            }
        } else if path.starts_with(&self.global_dir) {
            "global"
        } else {
            "session"
        }
    }

    /// Path to the workspace sessions directory.
    pub fn sessions_dir(&self) -> PathBuf {
        self.workspace_dir.join("sessions")
    }

    /// Write a daily session log file.
    /// File path: `~/.grok/memory/{project}-{hash8}/sessions/YYYY-MM-DD-{slug}-{sid8}.md`
    /// `date`: e.g. `"2026-02-23"`; `slug`: short slug derived from the first user message; `session_id`: full session ID (first 8 chars used as suffix); `append`: when `true`, appends a timestamped section instead of overwriting. Each section is separated by `---` and a timestamp header so the chunker treats them as distinct entries.
    pub fn write_daily_log(
        &self,
        date: &str,
        slug: &str,
        session_id: &str,
        content: &str,
        append: bool,
    ) -> std::io::Result<PathBuf> {
        self.require_legacy("daily session logs")?;
        let sessions_dir = self.sessions_dir();
        let sid8 = session_id
            .get(..session_id.len().min(8))
            .unwrap_or(session_id);
        let filename = format!("{date}-{slug}-{sid8}.md");
        let path = sessions_dir.join(&filename);

        if self.ephemeral {
            tracing::debug!(path = %path.display(), "MEMORY_EPHEMERAL_SKIP: daily log write skipped");
            return Ok(path);
        }

        std::fs::create_dir_all(&sessions_dir)?;

        if append && path.exists() {
            use std::io::Write;
            let timestamp = chrono::Utc::now().format("%H:%M:%S UTC");
            let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
            write!(file, "\n\n---\n\n<!-- flush {timestamp} -->\n\n{content}")?;
        } else {
            std::fs::write(&path, content)?;
        }
        tracing::debug!(path = %path.display(), append, "wrote daily session log");

        Ok(path)
    }

    /// Write the curated long-term `MEMORY.md` for the given scope.
    ///
    /// Creates parent directories as needed. Overwrites any existing content.
    pub fn write_long_term(&self, scope: MemoryScope, content: &str) -> std::io::Result<()> {
        self.require_legacy("direct MEMORY.md writes")?;
        if self.ephemeral && scope == MemoryScope::Workspace {
            tracing::debug!("MEMORY_EPHEMERAL_SKIP: workspace long-term write skipped");
            return Ok(());
        }

        let path = match scope {
            MemoryScope::Global => {
                std::fs::create_dir_all(&self.global_dir)?;
                self.global_memory_file()
            }
            MemoryScope::Workspace => {
                std::fs::create_dir_all(&self.workspace_dir)?;
                self.workspace_memory_file()
            }
        };

        std::fs::write(&path, content)?;
        tracing::debug!(path = %path.display(), scope = ?scope, "wrote long-term memory");

        Ok(())
    }

    /// Append content to the `MEMORY.md` for the given scope.
    /// Creates parent directories and the file if they don't exist.
    /// Empty/whitespace-only content is silently ignored.
    pub fn append_to_memory(&self, scope: MemoryScope, content: &str) -> std::io::Result<()> {
        self.require_legacy("direct MEMORY.md appends")?;
        if self.ephemeral && scope == MemoryScope::Workspace {
            tracing::debug!("MEMORY_EPHEMERAL_SKIP: workspace memory append skipped");
            return Ok(());
        }

        let normalized = normalize_memory_content(content);
        if normalized.is_empty() {
            return Ok(());
        }

        let path = match scope {
            MemoryScope::Global => {
                std::fs::create_dir_all(&self.global_dir)?;
                self.global_memory_file()
            }
            MemoryScope::Workspace => {
                std::fs::create_dir_all(&self.workspace_dir)?;
                self.workspace_memory_file()
            }
        };

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        use std::io::Write;
        if file.metadata()?.len() > 0 {
            write!(file, "\n\n{normalized}")?;
        } else {
            write!(file, "{normalized}")?;
        }

        tracing::debug!(path = %path.display(), scope = ?scope, "appended to memory");
        Ok(())
    }

    /// Save a user-authored remember note in the global memory scope.
    ///
    /// Legacy mode appends to `MEMORY.md`; v2 publishes a standalone immutable
    /// observation so it never mutates the generated manifest directly.
    pub fn save_remember_note(&self, content: &str) -> Result<(), SaveRememberNoteError> {
        let normalized = normalize_memory_content(content);
        if self.mode.is_v2() && normalized.len() > crate::v2::MAX_MANUAL_OBSERVATION_BYTES {
            return Err(SaveRememberNoteError::V2(
                crate::v2::V2StorageError::ObservationTooLarge {
                    actual_bytes: normalized.len(),
                    limit_bytes: crate::v2::MAX_MANUAL_OBSERVATION_BYTES,
                },
            ));
        }
        if normalized.is_empty() {
            return Ok(());
        }
        match self.mode {
            MemoryMode::Legacy => self
                .append_to_memory(MemoryScope::Global, &normalized)
                .map_err(SaveRememberNoteError::Legacy),
            MemoryMode::V2 => {
                let root = self.global_dir.parent().ok_or_else(|| {
                    SaveRememberNoteError::V2(crate::v2::V2StorageError::Io {
                        operation: "resolve v2 storage root",
                        path: self.global_dir.clone(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "v2 global directory has no storage root",
                        ),
                    })
                })?;
                crate::v2::ensure_scope_initialized(
                    root,
                    &self.global_dir,
                    crate::v2::V2MemoryScope::Global,
                )?;
                crate::v2::persist_observation(&self.global_dir, &normalized)
                    .map(|_| ())
                    .map_err(SaveRememberNoteError::V2)
            }
        }
    }

    /// Read a memory file, optionally returning only a range of lines.
    /// The path must resolve (via `canonicalize`) to a location inside the memory directory tree.
    /// Both the path and the memory root must be canonicalizable; if either fails, the read is rejected.
    pub fn read_file(
        &self,
        path: &Path,
        from: Option<usize>,
        lines: Option<usize>,
    ) -> std::io::Result<String> {
        // Security: canonicalize both sides; fail hard if either doesn't exist
        let canonical = dunce::canonicalize(path)?;
        let canonical_global = dunce::canonicalize(&self.global_dir).ok();
        let canonical_workspace = dunce::canonicalize(&self.workspace_dir).ok();
        if canonical_global.is_none() && canonical_workspace.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "memory directories {:?} and {:?} do not exist",
                    self.global_dir, self.workspace_dir
                ),
            ));
        }

        // Legacy nests workspaces below global_dir, so workspace_dir is never an independent trust root.
        // V2 keeps global/ and workspaces/ as siblings, but both canonical scope roots must stay below
        // the configured v2 root; otherwise a replaced scope symlink could admit files outside memory.
        let inside_memory = match self.mode {
            MemoryMode::Legacy => {
                canonical_global.is_some_and(|global| canonical.starts_with(global))
            }
            MemoryMode::V2 => self
                .global_dir
                .parent()
                .and_then(|root| dunce::canonicalize(root).ok())
                .is_some_and(|root| {
                    canonical_global.is_some_and(|global| {
                        global.starts_with(&root) && canonical.starts_with(global)
                    }) || canonical_workspace.is_some_and(|workspace| {
                        workspace.starts_with(&root) && canonical.starts_with(workspace)
                    })
                }),
        };
        // Fail-closed caveat for paths longer than MAX_PATH: see workspace clippy.toml
        if !inside_memory {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "path {:?} is outside the memory directories {:?} and {:?}",
                    path, self.global_dir, self.workspace_dir
                ),
            ));
        }

        // Read the canonicalized path, not the original, to prevent TOCTOU races.
        let content = std::fs::read_to_string(&canonical)?;

        let from = from.unwrap_or(0);
        match lines {
            Some(count) => {
                let selected: Vec<&str> = content.lines().skip(from).take(count).collect();
                Ok(selected.join("\n"))
            }
            None if from > 0 => {
                let selected: Vec<&str> = content.lines().skip(from).collect();
                Ok(selected.join("\n"))
            }
            None => Ok(content),
        }
    }

    /// List all memory files (`.md`) across global and workspace directories.
    ///
    /// Returns paths sorted by scope: global files first, then workspace files.
    pub fn list_memory_files(&self) -> std::io::Result<Vec<PathBuf>> {
        if self.mode.is_v2() {
            return self.list_v2_memory_files();
        }
        let mut files = Vec::new();

        // Global MEMORY.md
        let global_file = self.global_memory_file();
        if global_file.is_file() {
            files.push(global_file);
        }

        // Workspace MEMORY.md
        let workspace_file = self.workspace_memory_file();
        if workspace_file.is_file() {
            files.push(workspace_file);
        }

        // Workspace session logs
        let sessions_dir = self.sessions_dir();
        if sessions_dir.is_dir() {
            let mut session_files: Vec<PathBuf> = std::fs::read_dir(&sessions_dir)?
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("md") {
                        Some(path)
                    } else {
                        None
                    }
                })
                .collect();
            // Sort session logs by name (date-based, so chronological order).
            session_files.sort();
            files.extend(session_files);
        }

        Ok(files)
    }

    /// Ensure the global memory directory exists and create a template `MEMORY.md` if one doesn't already exist.
    ///
    /// Called on first run with memory enabled to bootstrap the layout.
    pub fn ensure_initialized(&self) -> std::io::Result<()> {
        if self.mode.is_v2() {
            let root = self.global_dir.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "v2 global directory has no storage root",
                )
            })?;
            crate::v2::ensure_scope_initialized(
                root,
                &self.global_dir,
                crate::v2::V2MemoryScope::Global,
            )
            .map_err(std::io::Error::other)?;
            if !self.ephemeral {
                crate::v2::ensure_scope_initialized(
                    root,
                    &self.workspace_dir,
                    crate::v2::V2MemoryScope::Workspace,
                )
                .map_err(std::io::Error::other)?;
            }
            return Ok(());
        }
        std::fs::create_dir_all(&self.global_dir)?;

        let global_file = self.global_memory_file();
        if !global_file.exists() {
            std::fs::write(
                &global_file,
                "# Global Memory\n\
                 \n\
                 > This file is automatically managed by Grok's memory system.\n\
                 > You can also edit it manually — changes will be indexed on next session.\n\
                 \n\
                 ## Preferences\n\
                 \n\
                 <!-- Add any cross-project preferences here -->\n",
            )?;
            tracing::info!(path = %global_file.display(), "created global MEMORY.md template");
        }

        if self.ephemeral {
            tracing::debug!("MEMORY_EPHEMERAL_SKIP: workspace initialization skipped");
            return Ok(());
        }

        std::fs::create_dir_all(&self.workspace_dir)?;

        let workspace_file = self.workspace_memory_file();
        if !workspace_file.exists() {
            std::fs::write(
                &workspace_file,
                format!(
                    "# Project Memory — {}\n\
                     \n\
                     > Auto-populated by dream consolidation. Edit freely.\n",
                    self.workspace_path.display()
                ),
            )?;
            tracing::info!(
                path = %workspace_file.display(),
                workspace = %self.workspace_path.display(),
                "created workspace MEMORY.md template"
            );
        }

        Ok(())
    }

    /// Remove the entire workspace-scoped memory directory.
    /// Deletes MEMORY.md, sessions/, index.sqlite, and any other workspace files.
    /// The directory will be recreated on next session start via `ensure_initialized()`.
    pub fn clear_workspace(&self) -> std::io::Result<bool> {
        match std::fs::remove_dir_all(&self.workspace_dir) {
            Ok(()) => {
                tracing::info!(path = %self.workspace_dir.display(), "cleared workspace memory");
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Remove the global MEMORY.md file.
    /// Does not remove the global memory directory itself (other workspaces may have subdirectories there).
    /// The file will be recreated on next session start via `ensure_initialized()`.
    pub fn clear_global(&self) -> std::io::Result<bool> {
        if self.mode.is_v2() {
            return match std::fs::remove_dir_all(&self.global_dir) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            };
        }
        let path = self.global_memory_file();
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "cleared global memory");
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Remove orphaned workspace directories under the memory root.
    /// `tmp*` dirs: remove empty ones unconditionally; remove non-empty ones older than 7 days; Other workspaces with no session files: remove if older than `max_age_days`; Non-empty non-tmp workspaces: never touched.
    pub fn gc(&self, max_age_days: u64) -> std::io::Result<usize> {
        if self.mode.is_v2() {
            return Ok(0);
        }
        let entries = match std::fs::read_dir(&self.global_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };

        let mut removed = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path == self.workspace_dir {
                continue;
            }

            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };

            let is_tmp = name.starts_with("tmp");
            let empty = is_empty_workspace(&path);

            let should_remove = if is_tmp {
                empty || is_older_than(&path, 7)
            } else {
                empty && is_older_than(&path, max_age_days)
            };

            if should_remove {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => {
                        tracing::debug!(
                            path = %path.display(),
                            is_tmp,
                            empty,
                            "MEMORY_GC: removed orphaned workspace directory"
                        );
                        removed += 1;
                    }
                    Err(e) => {
                        tracing::debug!(
                            path = %path.display(),
                            error = %e,
                            "MEMORY_GC: failed to remove workspace directory"
                        );
                    }
                }
            }
        }

        Ok(removed)
    }

    fn require_legacy(&self, operation: &str) -> std::io::Result<()> {
        if self.mode.is_legacy() {
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("{operation} are disabled in memory v2"),
        ))
    }

    fn list_v2_memory_files(&self) -> std::io::Result<Vec<PathBuf>> {
        crate::storage_v2::list_memory_files(&self.global_dir, &self.workspace_dir)
    }
}

/// A workspace directory is "empty" if its `sessions/` subdirectory either does not exist or contains no entries.
fn is_empty_workspace(dir: &Path) -> bool {
    let sessions = dir.join("sessions");
    if !sessions.is_dir() {
        return true;
    }
    match std::fs::read_dir(&sessions) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => true,
    }
}

/// Returns `true` if `dir`'s mtime is older than `days` days ago.
fn is_older_than(dir: &Path, days: u64) -> bool {
    let Ok(metadata) = dir.metadata() else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let age = modified.elapsed().unwrap_or(std::time::Duration::ZERO);
    age > std::time::Duration::from_secs(days * 24 * 60 * 60)
}

/// Ensure content has proper Markdown heading structure for the memory chunker.
/// Content that already starts with `#` is left as-is (user-provided structure); Single-line content becomes `## {content}` (the note IS the heading); Multi-line with a first line of 80 chars or fewer: the first line becomes `## {first_line}`, the rest becomes the body paragraph; Multi-line with a longer first line: the heading is a generic `## Note` and the entire content becomes the body.
pub fn normalize_memory_content(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Already has a Markdown heading, so preserve the user's structure
    if trimmed.starts_with('#') {
        return trimmed.to_string();
    }

    match trimmed.find('\n') {
        // Single-line: the note IS the heading
        None => format!("## {trimmed}"),

        // Multi-line: promote the first line to a heading if it's short enough
        Some(pos) => {
            let first_line = trimmed.get(..pos).unwrap_or("").trim();
            let rest = trimmed.get(pos..).unwrap_or("").trim();

            if first_line.len() <= 80 {
                format!("## {first_line}\n\n{rest}")
            } else {
                format!("## Note\n\n{trimmed}")
            }
        }
    }
}

/// Returns `true` if `cwd` resides under a system temp directory.
/// Subagent worktrees and other transient processes use temp-dir paths like `/tmp/…` or `/var/folders/…/T/…`.
/// Creating persistent workspace memory for these paths is wasteful and produces orphan directories.
fn is_ephemeral_cwd(cwd: &Path) -> bool {
    let canonical = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let s = canonical.to_string_lossy();
    let raw_s = cwd.to_string_lossy();

    let temp = std::env::temp_dir();
    let temp_canonical = dunce::canonicalize(&temp).unwrap_or(temp);

    canonical.starts_with(&temp_canonical)
        || raw_s.starts_with("/tmp/")
        || raw_s.starts_with("/var/tmp/")
        || (raw_s.contains("/var/folders/") && raw_s.contains("/T/"))
        || s.starts_with("/private/tmp/")
        || s.starts_with("/private/var/tmp/")
        || (s.contains("/private/var/folders/") && s.contains("/T/"))
}

/// Compute a human-friendly workspace directory name.
/// It falls back to the filesystem path when not inside a git repo or when no `origin` remote is configured.
fn compute_workspace_hash(cwd: &Path) -> String {
    let identity = extract_repo_identity(cwd);

    let (slug, hash_input) = match identity {
        Some(ref repo_id) => {
            let slug_source = repo_id.rsplit('/').next().unwrap_or(repo_id);
            (slugify(slug_source, 40), repo_id.as_str().to_string())
        }
        None => {
            // Windows-only, non-git cwds: dunce changes the hash input, so the old-form dir is orphaned until gc() reaps it after max_age_days
            // That orphan is accepted over an unverifiable rename migration (Unix unchanged)
            let canonical = dunce::canonicalize(cwd).unwrap_or_else(|_| {
                tracing::warn!(
                    path = %cwd.display(),
                    "could not canonicalize workspace path for memory hash; using raw path"
                );
                cwd.to_path_buf()
            });
            let dir_name = canonical
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace");
            (
                slugify(dir_name, 40),
                canonical.to_string_lossy().to_string(),
            )
        }
    };

    let slug = if slug.is_empty() { "workspace" } else { &slug };
    let hash = blake3::hash(hash_input.as_bytes());
    let hex = hash.to_hex();
    let hash8 = hex.get(..8).unwrap_or(&*hex);

    format!("{slug}-{hash8}")
}

/// Extract a normalized `org/repo` identifier from the git remote URL.
/// Uses `git2` to discover the repository from `cwd` and read the `origin` remote URL.
/// Returns `None` if not a git repo, no `origin` remote, or the URL can't be normalized.
pub(crate) fn extract_repo_identity(cwd: &Path) -> Option<String> {
    let repo = git2::Repository::discover(cwd).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    let url = remote.url()?;
    normalize_remote_url(url)
}

/// Normalize a git remote URL to `org/repo` form.
/// Strips protocol prefix, host, and trailing `.git`:
fn normalize_remote_url(url: &str) -> Option<String> {
    let path = if let Some(colon_pos) = url.find(':') {
        // SSH format: git@github.com:org/repo.git
        if url
            .get(..colon_pos)
            .is_some_and(|h| h.contains('@') && !h.contains('/'))
        {
            url.get(colon_pos + 1..)?
        } else {
            // HTTPS/SSH-with-scheme: https://github.com/org/repo.git
            url.split("//")
                .nth(1)
                .and_then(|after_scheme| after_scheme.split_once('/'))
                .map(|(_, path)| path)?
        }
    } else {
        return None;
    };

    let cleaned = path
        .trim_end_matches(".git")
        .trim_end_matches('/')
        .trim_start_matches('/');

    if cleaned.is_empty() || !cleaned.contains('/') {
        return None;
    }

    Some(cleaned.to_string())
}

/// Generate a URL-safe slug (e.g., from the first user message): lowercase, non-alphanumerics become `-`, consecutive dashes collapse.
/// Truncates to `max_len` characters (not bytes) and strips leading/trailing `-`.
pub fn slugify(input: &str, max_len: usize) -> String {
    let slug: String = input
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive dashes
    let mut result = String::with_capacity(slug.len());
    let mut prev_dash = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_dash {
                result.push('-');
            }
            prev_dash = true;
        } else {
            result.push(c);
            prev_dash = false;
        }
    }

    // Truncate by char count (safe for multi-byte), then trim dashes
    let truncated: String = result.chars().take(max_len).collect();
    truncated.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Prepend the hermetic git binary (via `GIT_BIN_PATH`) to `PATH`.
    /// `Command::new("git")` and `git2`'s discovery then resolve to the hermetic static binary instead of system-installed git.
    /// Safe to call multiple times; only the first call mutates `PATH`.
    fn ensure_hermetic_git_on_path() {
        use std::path::PathBuf;
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            if let Ok(git_bin) = std::env::var("GIT_BIN_PATH") {
                let p = PathBuf::from(&git_bin);
                let p = if p.is_relative() {
                    std::env::current_dir().unwrap().join(&p)
                } else {
                    p
                };
                if let Some(dir) = p.parent() {
                    let cur = std::env::var("PATH").unwrap_or_default();
                    unsafe {
                        std::env::set_var("PATH", format!("{}:{}", dir.display(), cur));
                    }
                }
            }
        });
    }

    #[test]
    fn test_compute_workspace_hash_deterministic() {
        let hash1 = compute_workspace_hash(Path::new("/some/workspace"));
        let hash2 = compute_workspace_hash(Path::new("/some/workspace"));
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_compute_workspace_hash_human_readable() {
        let name = compute_workspace_hash(Path::new("/users/me/work/xai"));
        assert!(
            name.starts_with("xai-"),
            "should start with project name slug, got: {name}"
        );
        // Format: {slug}-{8 hex chars}
        let parts: Vec<&str> = name.rsplitn(2, '-').collect();
        let Some(suffix) = parts.first() else {
            panic!("expected hash suffix: {name}");
        };
        assert_eq!(
            suffix.len(),
            8,
            "hash suffix should be 8 hex chars, got: {name}"
        );
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "hash suffix should be hex, got: {name}"
        );
    }

    #[test]
    fn test_compute_workspace_hash_different_paths() {
        let hash1 = compute_workspace_hash(Path::new("/workspace/a"));
        let hash2 = compute_workspace_hash(Path::new("/workspace/b"));
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_compute_workspace_hash_same_dirname_different_parent() {
        // Two "app" dirs in different parents get different names (hash differs)
        let name1 = compute_workspace_hash(Path::new("/users/alice/app"));
        let name2 = compute_workspace_hash(Path::new("/users/bob/app"));
        assert!(name1.starts_with("app-"), "got: {name1}");
        assert!(name2.starts_with("app-"), "got: {name2}");
        assert_ne!(
            name1, name2,
            "same dir name but different parents should differ"
        );
    }

    #[test]
    fn test_slugify_basic() {
        assert_eq!(slugify("Hello World", 20), "hello-world");
    }

    #[test]
    fn test_slugify_special_chars() {
        assert_eq!(
            slugify("Fix the bug in auth/login.rs", 30),
            "fix-the-bug-in-auth-login-rs"
        );
    }

    #[test]
    fn test_slugify_truncation() {
        assert_eq!(slugify("a very long message here", 10), "a-very-lon");
    }

    #[test]
    fn test_slugify_leading_trailing_dashes() {
        assert_eq!(slugify("---hello---", 20), "hello");
    }

    #[test]
    fn test_slugify_consecutive_special_chars() {
        assert_eq!(slugify("hello!!!world", 20), "hello-world");
    }

    #[test]
    fn test_slugify_empty() {
        assert_eq!(slugify("", 20), "");
    }

    #[test]
    fn test_storage_write_and_read_daily_log() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        let content = "## Session Summary\n\nWorked on feature X.";
        let path = storage
            .write_daily_log("2026-02-23", "fix-auth", "session12345678", content, false)
            .unwrap();

        assert!(path.exists());
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("2026-02-23-fix-auth-session1")
        );

        let read_back = storage.read_file(&path, None, None).unwrap();
        assert_eq!(read_back, content);
    }

    #[test]
    fn test_storage_read_file_with_line_range() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        let content = "line 0\nline 1\nline 2\nline 3\nline 4";
        let path = storage
            .write_daily_log("2026-02-23", "test", "sess12345678", content, false)
            .unwrap();

        // Read lines 1..3 (0-indexed)
        let partial = storage.read_file(&path, Some(1), Some(2)).unwrap();
        assert_eq!(partial, "line 1\nline 2");
    }

    #[test]
    fn test_storage_write_long_term_global() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        storage
            .write_long_term(MemoryScope::Global, "# Global\n\nSome knowledge.")
            .unwrap();

        let path = global_dir.join("MEMORY.md");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "# Global\n\nSome knowledge.");
    }

    #[test]
    fn test_storage_write_long_term_workspace() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir.clone());

        storage
            .write_long_term(MemoryScope::Workspace, "# Project\n\nProject info.")
            .unwrap();

        let path = workspace_dir.join("MEMORY.md");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "# Project\n\nProject info.");
    }

    #[test]
    fn test_storage_list_memory_files() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        // Initially empty
        let files = storage.list_memory_files().unwrap();
        assert!(files.is_empty());

        // Write some files
        storage
            .write_long_term(MemoryScope::Global, "global")
            .unwrap();
        storage
            .write_long_term(MemoryScope::Workspace, "workspace")
            .unwrap();
        storage
            .write_daily_log("2026-02-23", "test", "sess12345678", "session log", false)
            .unwrap();

        let files = storage.list_memory_files().unwrap();
        assert_eq!(files.len(), 3);

        // Global MEMORY.md comes first
        let Some(first) = files.first() else {
            panic!("expected listed memory files: {files:?}");
        };
        assert!(first.ends_with("MEMORY.md"));
        assert!(
            first
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                == "memory"
        );
    }

    #[test]
    fn test_storage_ensure_initialized() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir.clone());

        storage.ensure_initialized().unwrap();

        assert!(global_dir.join("MEMORY.md").exists());
        assert!(workspace_dir.join("MEMORY.md").exists());

        // Calling again is idempotent (does not overwrite)
        let content_before = std::fs::read_to_string(global_dir.join("MEMORY.md")).unwrap();
        storage.ensure_initialized().unwrap();
        let content_after = std::fs::read_to_string(global_dir.join("MEMORY.md")).unwrap();
        assert_eq!(content_before, content_after);
    }

    #[test]
    fn test_storage_read_file_rejects_outside_path() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        std::fs::create_dir_all(&global_dir).unwrap();
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        // Try to read a file outside the memory directory
        let outside = tmp.path().join("outside.md");
        std::fs::write(&outside, "secret").unwrap();

        let result = storage.read_file(&outside, None, None);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn test_storage_daily_log_overwrites() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        let path1 = storage
            .write_daily_log("2026-02-23", "test", "sess12345678", "first", false)
            .unwrap();
        let path2 = storage
            .write_daily_log("2026-02-23", "test", "sess12345678", "second", false)
            .unwrap();

        assert_eq!(path1, path2);
        let content = std::fs::read_to_string(&path2).unwrap();
        assert_eq!(content, "second");
    }

    #[test]
    fn test_storage_daily_log_append() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        let path1 = storage
            .write_daily_log("2026-02-23", "flush", "sess12345678", "## First", true)
            .unwrap();
        // First write to a new file uses write (no separator).
        let content = std::fs::read_to_string(&path1).unwrap();
        assert_eq!(content, "## First");

        let path2 = storage
            .write_daily_log("2026-02-23", "flush", "sess12345678", "## Second", true)
            .unwrap();
        assert_eq!(path1, path2);

        let content = std::fs::read_to_string(&path2).unwrap();
        assert!(
            content.starts_with("## First"),
            "original content preserved"
        );
        assert!(content.contains("---"), "separator present");
        assert!(content.contains("<!-- flush"), "timestamp marker present");
        assert!(content.contains("## Second"), "appended content present");
    }

    // -----------------------------------------------------------------------
    // normalize_memory_content tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_single_line() {
        assert_eq!(
            normalize_memory_content("prefer tabs over spaces"),
            "## prefer tabs over spaces"
        );
    }

    #[test]
    fn test_normalize_already_has_h2_heading() {
        let input = "## Build conventions\n\nCargo workspace, run clippy before push";
        assert_eq!(normalize_memory_content(input), input);
    }

    #[test]
    fn test_normalize_already_has_h1_heading() {
        let input = "# Top Level\n\nSome body text.";
        assert_eq!(normalize_memory_content(input), input);
    }

    #[test]
    fn test_normalize_multiline_short_first_line() {
        assert_eq!(
            normalize_memory_content("This project uses React 18\nThe build system is Vite"),
            "## This project uses React 18\n\nThe build system is Vite"
        );
    }

    #[test]
    fn test_normalize_multiline_long_first_line() {
        let long_line = "a".repeat(81);
        let input = format!("{long_line}\nMore details here");
        let result = normalize_memory_content(&input);
        assert!(result.starts_with("## Note\n\n"));
        assert!(result.contains(&long_line));
        assert!(result.contains("More details here"));
    }

    #[test]
    fn test_normalize_multiline_exactly_80_chars() {
        let line_80 = "a".repeat(80);
        let input = format!("{line_80}\nbody");
        let result = normalize_memory_content(&input);
        assert!(
            result.starts_with("## aaaa"),
            "80-char first line should be promoted to heading"
        );
        assert!(!result.starts_with("## Note"));
    }

    #[test]
    fn test_normalize_empty() {
        assert_eq!(normalize_memory_content(""), "");
    }

    #[test]
    fn test_normalize_whitespace_only() {
        assert_eq!(normalize_memory_content("   \n  \n  "), "");
    }

    #[test]
    fn test_normalize_trims_surrounding_whitespace() {
        assert_eq!(
            normalize_memory_content("  prefer tabs  "),
            "## prefer tabs"
        );
    }

    #[test]
    fn test_normalize_preserves_internal_newlines() {
        let input = "First line\nSecond line\nThird line";
        let result = normalize_memory_content(input);
        assert_eq!(result, "## First line\n\nSecond line\nThird line");
    }

    // -----------------------------------------------------------------------
    // append_to_memory tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_append_to_memory_workspace_empty_file() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir.clone());

        storage
            .append_to_memory(MemoryScope::Workspace, "prefer tabs")
            .unwrap();

        let content = std::fs::read_to_string(workspace_dir.join("MEMORY.md")).unwrap();
        assert_eq!(content, "## prefer tabs");
    }

    #[test]
    fn test_append_to_memory_global() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        storage
            .append_to_memory(MemoryScope::Global, "always use UTC")
            .unwrap();

        let content = std::fs::read_to_string(global_dir.join("MEMORY.md")).unwrap();
        assert_eq!(content, "## always use UTC");
    }

    #[test]
    fn test_append_to_memory_adds_separator() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir.clone());

        storage
            .append_to_memory(MemoryScope::Workspace, "first note")
            .unwrap();
        storage
            .append_to_memory(MemoryScope::Workspace, "second note")
            .unwrap();

        let content = std::fs::read_to_string(workspace_dir.join("MEMORY.md")).unwrap();
        assert_eq!(content, "## first note\n\n## second note");
    }

    #[test]
    fn test_append_to_memory_normalizes_content() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir.clone());

        storage
            .append_to_memory(MemoryScope::Workspace, "raw text without heading")
            .unwrap();

        let content = std::fs::read_to_string(workspace_dir.join("MEMORY.md")).unwrap();
        assert!(
            content.starts_with("## "),
            "should have been normalized with a heading"
        );
    }

    #[test]
    fn test_append_to_memory_preserves_user_heading() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir.clone());

        storage
            .append_to_memory(
                MemoryScope::Workspace,
                "## My Custom Heading\n\nDetails here.",
            )
            .unwrap();

        let content = std::fs::read_to_string(workspace_dir.join("MEMORY.md")).unwrap();
        assert_eq!(content, "## My Custom Heading\n\nDetails here.");
    }

    #[test]
    fn test_append_to_memory_ignores_empty_content() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir.clone());

        storage
            .append_to_memory(MemoryScope::Workspace, "   ")
            .unwrap();

        assert!(!workspace_dir.join("MEMORY.md").exists());
    }

    // -----------------------------------------------------------------------
    // clear_workspace / clear_global tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_clear_workspace_removes_directory() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir.clone());

        storage.ensure_initialized().unwrap();
        storage
            .write_daily_log("2026-05-05", "test", "sess12345678", "log content", false)
            .unwrap();
        assert!(workspace_dir.is_dir());
        assert!(workspace_dir.join("MEMORY.md").exists());

        let removed = storage.clear_workspace().unwrap();
        assert!(removed);
        assert!(!workspace_dir.exists());
    }

    #[test]
    fn test_clear_workspace_returns_false_when_missing() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("nonexistent");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        let removed = storage.clear_workspace().unwrap();
        assert!(!removed);
    }

    #[test]
    fn test_clear_global_removes_file_but_not_directory() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        storage.ensure_initialized().unwrap();
        assert!(global_dir.join("MEMORY.md").exists());

        let removed = storage.clear_global().unwrap();
        assert!(removed);
        assert!(!global_dir.join("MEMORY.md").exists());
        assert!(global_dir.is_dir(), "global directory itself should remain");
    }

    #[test]
    fn test_clear_global_returns_false_when_missing() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        std::fs::create_dir_all(&global_dir).unwrap();
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        let removed = storage.clear_global().unwrap();
        assert!(!removed);
    }

    #[test]
    fn test_clear_workspace_then_reinitialize() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("abc123");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir.clone());

        storage.ensure_initialized().unwrap();
        storage
            .append_to_memory(MemoryScope::Workspace, "custom entry")
            .unwrap();
        let before = std::fs::read_to_string(workspace_dir.join("MEMORY.md")).unwrap();
        assert!(before.contains("custom entry"));

        storage.clear_workspace().unwrap();
        storage.ensure_initialized().unwrap();

        let after = std::fs::read_to_string(workspace_dir.join("MEMORY.md")).unwrap();
        assert!(
            !after.contains("custom entry"),
            "reinitialized file should be a fresh template"
        );
        assert!(after.contains("Project Memory"));
    }

    // -----------------------------------------------------------------------
    // normalize_remote_url tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_ssh_url() {
        assert_eq!(
            normalize_remote_url("git@github.com:acme/widgets.git"),
            Some("acme/widgets".to_string())
        );
    }

    #[test]
    fn test_normalize_https_url() {
        assert_eq!(
            normalize_remote_url("https://github.com/acme/widgets.git"),
            Some("acme/widgets".to_string())
        );
    }

    #[test]
    fn test_normalize_https_no_dot_git() {
        assert_eq!(
            normalize_remote_url("https://github.com/acme/widgets"),
            Some("acme/widgets".to_string())
        );
    }

    #[test]
    fn test_normalize_ssh_with_scheme() {
        assert_eq!(
            normalize_remote_url("ssh://git@github.com/acme/widgets"),
            Some("acme/widgets".to_string())
        );
    }

    #[test]
    fn test_normalize_self_hosted() {
        assert_eq!(
            normalize_remote_url("git@gitlab.example.com:team/project.git"),
            Some("team/project".to_string())
        );
    }

    #[test]
    fn test_normalize_no_org_returns_none() {
        assert_eq!(normalize_remote_url("git@github.com:widgets.git"), None);
    }

    #[test]
    fn test_normalize_no_colon_returns_none() {
        assert_eq!(normalize_remote_url("just-a-path"), None);
    }

    #[test]
    fn test_normalize_empty_returns_none() {
        assert_eq!(normalize_remote_url(""), None);
    }

    #[test]
    fn test_normalize_deep_path() {
        assert_eq!(
            normalize_remote_url("https://github.com/acme/tools/sub.git"),
            Some("acme/tools/sub".to_string())
        );
    }

    #[test]
    fn test_normalize_protocols_produce_same_identity() {
        let ssh = normalize_remote_url("git@github.com:acme/widgets.git");
        let https = normalize_remote_url("https://github.com/acme/widgets.git");
        let ssh_scheme = normalize_remote_url("ssh://git@github.com/acme/widgets.git");
        assert_eq!(ssh, https);
        assert_eq!(https, ssh_scheme);
    }

    // -----------------------------------------------------------------------
    // extract_repo_identity tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_repo_identity_from_current_repo() {
        ensure_hermetic_git_on_path();
        let tmp = TempDir::new().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        repo.remote("origin", "git@github.com:example/demo.git")
            .unwrap();

        let identity = extract_repo_identity(tmp.path());
        assert!(
            identity.is_some(),
            "should detect repo identity from git directory with origin remote"
        );
        let id = identity.unwrap();
        assert_eq!(id, "example/demo");
    }

    #[test]
    fn test_extract_repo_identity_non_git_dir() {
        let tmp = TempDir::new().unwrap();
        let identity = extract_repo_identity(tmp.path());
        assert_eq!(identity, None, "non-git directory should return None");
    }

    #[test]
    fn test_compute_workspace_hash_uses_repo_identity() {
        // Two different paths in the same repo should produce the same hash.
        ensure_hermetic_git_on_path();
        let tmp = TempDir::new().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        repo.remote("origin", "git@github.com:example/demo.git")
            .unwrap();

        let subdir = tmp.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let hash1 = compute_workspace_hash(tmp.path());
        let hash2 = compute_workspace_hash(&subdir);
        assert_eq!(
            hash1, hash2,
            "different subdirs of same repo should share identity"
        );
    }

    #[test]
    fn test_compute_workspace_hash_non_git_falls_back() {
        let tmp = TempDir::new().unwrap();
        let hash = compute_workspace_hash(tmp.path());
        // Should still produce a valid slug-hash format
        assert!(hash.contains('-'), "should have slug-hash format: {hash}");
        let parts: Vec<&str> = hash.rsplitn(2, '-').collect();
        assert_eq!(
            parts.first().map_or(0, |p| p.len()),
            8,
            "hash suffix should be 8 hex chars"
        );
    }

    // -----------------------------------------------------------------------
    // is_ephemeral_cwd tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_ephemeral_linux_tmp() {
        assert!(is_ephemeral_cwd(Path::new("/tmp/foo")));
        assert!(is_ephemeral_cwd(Path::new("/tmp/subagent-worktree-123")));
    }

    #[test]
    fn test_ephemeral_linux_var_tmp() {
        assert!(is_ephemeral_cwd(Path::new("/var/tmp/bar")));
        assert!(is_ephemeral_cwd(Path::new("/var/tmp/nested/deep")));
    }

    #[test]
    fn test_ephemeral_macos_var_folders() {
        assert!(is_ephemeral_cwd(Path::new(
            "/var/folders/xx/yyyyyyyy/T/tmpABCDEF"
        )));
        assert!(is_ephemeral_cwd(Path::new(
            "/private/var/folders/xx/yyyyyyyy/T/tmpABCDEF"
        )));
    }

    #[test]
    fn test_ephemeral_macos_private_tmp() {
        assert!(is_ephemeral_cwd(Path::new("/private/tmp/foo")));
        assert!(is_ephemeral_cwd(Path::new("/private/var/tmp/bar")));
    }

    #[test]
    fn test_non_ephemeral_normal_paths() {
        assert!(!is_ephemeral_cwd(Path::new("/home/user/project")));
        assert!(!is_ephemeral_cwd(Path::new("/home/user/src")));
        assert!(!is_ephemeral_cwd(Path::new("/Users/dev/work/repo")));
        assert!(!is_ephemeral_cwd(Path::new("/opt/workspace")));
    }

    #[test]
    fn test_ephemeral_storage_skips_workspace_writes() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("ephemeral-abc12345");

        let storage = MemoryStorage {
            mode: MemoryMode::Legacy,
            global_dir: global_dir.clone(),
            workspace_dir: workspace_dir.clone(),
            workspace_path: PathBuf::from("/tmp/test"),
            ephemeral: true,
        };

        // write_daily_log returns Ok but must not create the file
        let path = storage
            .write_daily_log("2026-05-07", "test", "sess12345678", "content", false)
            .unwrap();
        assert!(!path.exists());

        // write_long_term for workspace should no-op
        storage
            .write_long_term(MemoryScope::Workspace, "should not write")
            .unwrap();
        assert!(!workspace_dir.join("MEMORY.md").exists());

        // write_long_term for global should still work
        storage
            .write_long_term(MemoryScope::Global, "global content")
            .unwrap();
        assert!(global_dir.join("MEMORY.md").exists());
    }

    #[test]
    fn test_ephemeral_storage_skips_append() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("ephemeral-abc12345");

        let storage = MemoryStorage {
            mode: MemoryMode::Legacy,
            global_dir: global_dir.clone(),
            workspace_dir: workspace_dir.clone(),
            workspace_path: PathBuf::from("/tmp/test"),
            ephemeral: true,
        };

        // Workspace append should be skipped
        storage
            .append_to_memory(MemoryScope::Workspace, "should skip")
            .unwrap();
        assert!(!workspace_dir.join("MEMORY.md").exists());

        // Global append should still work
        storage
            .append_to_memory(MemoryScope::Global, "global note")
            .unwrap();
        assert!(global_dir.join("MEMORY.md").exists());
    }

    #[test]
    fn test_ephemeral_storage_skips_workspace_init() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("ephemeral-abc12345");

        let storage = MemoryStorage {
            mode: MemoryMode::Legacy,
            global_dir: global_dir.clone(),
            workspace_dir: workspace_dir.clone(),
            workspace_path: PathBuf::from("/tmp/test"),
            ephemeral: true,
        };

        storage.ensure_initialized().unwrap();

        assert!(global_dir.join("MEMORY.md").exists());
        assert!(!workspace_dir.exists());
    }

    #[test]
    fn test_ephemeral_flag_set_via_new() {
        // A temp-dir CWD should produce an ephemeral storage
        let storage = MemoryStorage::new(Path::new("/tmp/fake-worktree"), None);
        assert!(storage.is_ephemeral());

        // A normal CWD should not
        let storage = MemoryStorage::new(Path::new("/home/user/project"), None);
        assert!(!storage.is_ephemeral());
    }

    #[test]
    fn test_new_flat_never_ephemeral() {
        // new_flat uses use_workspace_hash=false, so ephemeral should always be false
        let storage = MemoryStorage::new_flat(Path::new("/tmp/something"), Path::new("/tmp/root"));
        assert!(!storage.is_ephemeral());
    }

    // -----------------------------------------------------------------------
    // gc tests
    // -----------------------------------------------------------------------

    fn set_dir_mtime_days_ago(dir: &Path, days: u64) {
        let t =
            std::time::SystemTime::now() - std::time::Duration::from_secs(days * 24 * 60 * 60 + 60);
        filetime::set_file_mtime(dir, filetime::FileTime::from_system_time(t)).unwrap();
    }

    #[test]
    fn test_gc_empty_tmp_removed_unconditionally() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // Create an empty tmp dir (no sessions subdir)
        std::fs::create_dir_all(global_dir.join("tmp-abc12345")).unwrap();

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 1);
        assert!(!global_dir.join("tmp-abc12345").exists());
    }

    #[test]
    fn test_gc_nonempty_tmp_young_kept() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // Create a non-empty tmp dir that is young (mtime = now)
        let tmp_ws = global_dir.join("tmp-def12345");
        let sessions = tmp_ws.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("2026-05-01-test-sess1234.md"), "log").unwrap();

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 0);
        assert!(tmp_ws.exists());
    }

    #[test]
    fn test_gc_nonempty_tmp_old_removed() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // Create a non-empty tmp dir that is old (over 7 days)
        let tmp_ws = global_dir.join("tmp-ghi12345");
        let sessions = tmp_ws.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("2026-04-01-old-sess1234.md"), "log").unwrap();
        set_dir_mtime_days_ago(&tmp_ws, 8);

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 1);
        assert!(!tmp_ws.exists());
    }

    #[test]
    fn test_gc_empty_workspace_old_removed() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // Create an empty workspace dir older than max_age_days
        let old_ws = global_dir.join("old-project-ab123456");
        std::fs::create_dir_all(&old_ws).unwrap();
        set_dir_mtime_days_ago(&old_ws, 31);

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 1);
        assert!(!old_ws.exists());
    }

    #[test]
    fn test_gc_empty_workspace_young_kept() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // Create an empty workspace dir younger than max_age_days
        let young_ws = global_dir.join("young-project-cd123456");
        std::fs::create_dir_all(&young_ws).unwrap();

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 0);
        assert!(young_ws.exists());
    }

    #[test]
    fn test_gc_nonempty_workspace_never_removed() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // Create a non-empty workspace dir that is old
        let active_ws = global_dir.join("active-project-ef123456");
        let sessions = active_ws.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("2026-01-01-work-sess1234.md"), "log").unwrap();
        set_dir_mtime_days_ago(&active_ws, 60);

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 0);
        assert!(active_ws.exists());
    }

    #[test]
    fn test_gc_skips_files_in_root() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // Create a file (not a directory) in the memory root
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(global_dir.join("MEMORY.md"), "global").unwrap();

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 0);
        assert!(global_dir.join("MEMORY.md").exists());
    }

    #[test]
    fn test_gc_nonexistent_root_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("does-not-exist");
        let workspace_dir = global_dir.join("ws");
        let storage = MemoryStorage::with_paths(global_dir, workspace_dir);

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_gc_returns_correct_count() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // 2 empty tmp dirs (removed unconditionally)
        std::fs::create_dir_all(global_dir.join("tmp-one-12345678")).unwrap();
        std::fs::create_dir_all(global_dir.join("tmp-two-12345678")).unwrap();

        // 1 empty old workspace (removed)
        let old = global_dir.join("old-ws-12345678");
        std::fs::create_dir_all(&old).unwrap();
        set_dir_mtime_days_ago(&old, 31);

        // 1 non-empty workspace (kept)
        let active = global_dir.join("active-12345678");
        let sessions = active.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("log.md"), "x").unwrap();

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 3);
    }

    #[test]
    fn test_gc_workspace_with_memory_md_but_no_sessions_is_empty() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        let workspace_dir = global_dir.join("current-ws");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir);

        // A workspace with MEMORY.md and index.sqlite but no sessions/
        let ws = global_dir.join("orphan-ab123456");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("MEMORY.md"), "# Project").unwrap();
        std::fs::write(ws.join("index.sqlite"), "").unwrap();
        set_dir_mtime_days_ago(&ws, 31);

        let removed = storage.gc(30).unwrap();
        assert_eq!(
            removed, 1,
            "workspace with MEMORY.md but no sessions is empty"
        );
        assert!(!ws.exists());
    }

    #[test]
    fn test_gc_skips_current_workspace() {
        let tmp = TempDir::new().unwrap();
        let global_dir = tmp.path().join("memory");
        // workspace_dir points at a real directory inside global_dir
        let workspace_dir = global_dir.join("my-project-ab123456");
        let storage = MemoryStorage::with_paths(global_dir.clone(), workspace_dir.clone());

        // Create the current workspace: old, no sessions, so it would qualify for GC
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("MEMORY.md"), "# My project").unwrap();
        set_dir_mtime_days_ago(&workspace_dir, 60);

        // Create another old empty workspace that SHOULD be removed
        let other = global_dir.join("other-cd123456");
        std::fs::create_dir_all(&other).unwrap();
        set_dir_mtime_days_ago(&other, 31);

        let removed = storage.gc(30).unwrap();
        assert_eq!(removed, 1, "only the other workspace should be removed");
        assert!(workspace_dir.exists(), "current workspace must survive GC");
        assert!(!other.exists());
    }

    // -----------------------------------------------------------------------
    // is_empty_workspace / is_older_than unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_empty_workspace_no_sessions_dir() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(is_empty_workspace(&ws));
    }

    #[test]
    fn test_is_empty_workspace_empty_sessions_dir() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("sessions")).unwrap();
        assert!(is_empty_workspace(&ws));
    }

    #[test]
    fn test_is_empty_workspace_with_session_files() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        let sessions = ws.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("2026-01-01-test-sess1234.md"), "log").unwrap();
        assert!(!is_empty_workspace(&ws));
    }

    #[test]
    fn test_is_older_than_new_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("fresh");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_older_than(&dir, 1));
    }

    #[test]
    fn test_is_older_than_old_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("old");
        std::fs::create_dir_all(&dir).unwrap();
        set_dir_mtime_days_ago(&dir, 10);
        assert!(is_older_than(&dir, 7));
        assert!(!is_older_than(&dir, 15));
    }

    #[test]
    fn test_total_chunk_count_missing_and_empty_index() {
        let tmp = TempDir::new().unwrap();
        let storage = MemoryStorage::with_paths(
            tmp.path().join("memory"),
            tmp.path().join("memory").join("test_ws"),
        );

        // A missing index counts as 0, and the journal-safe open must never create it
        assert_eq!(storage.total_chunk_count(), 0);
        assert!(!storage.workspace_dir().join("index.sqlite").exists());

        let _idx = crate::index::MemoryIndex::open_or_create(
            &storage.workspace_dir().join("index.sqlite"),
            storage.clone(),
            distill_config_types::MemoryIndexConfig::default(),
            64,
        )
        .unwrap();
        assert_eq!(storage.total_chunk_count(), 0);
    }

    #[test]
    fn v2_storage_is_fully_isolated_from_legacy_files() {
        let tmp = TempDir::new().unwrap();
        let legacy_root = tmp.path().join("memory");
        let v2_root = tmp.path().join("memory-v2");
        std::fs::create_dir_all(&legacy_root).unwrap();
        std::fs::write(legacy_root.join("MEMORY.md"), "legacy sentinel").unwrap();

        let storage = MemoryStorage::new_for_mode(
            Path::new("/home/user/project"),
            Some(&v2_root),
            MemoryMode::V2,
        );
        storage.ensure_initialized().unwrap();

        assert_eq!(storage.mode(), MemoryMode::V2);
        assert!(storage.global_dir().starts_with(&v2_root));
        assert!(
            storage
                .workspace_dir()
                .starts_with(v2_root.join("workspaces"))
        );
        assert_eq!(
            std::fs::read_to_string(legacy_root.join("MEMORY.md")).unwrap(),
            "legacy sentinel"
        );
        assert_eq!(
            storage
                .write_daily_log("2026-09-01", "forbidden", "session", "content", false)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
        assert_eq!(
            storage
                .write_long_term(MemoryScope::Workspace, "forbidden")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
        assert_eq!(
            storage
                .append_to_memory(MemoryScope::Global, "forbidden")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Unsupported
        );
        assert_eq!(storage.gc(0).unwrap(), 0);
        assert!(storage.global_memory_file().exists());
        assert!(storage.workspace_memory_file().exists());
        assert!(
            storage
                .read_file(&storage.global_memory_file(), None, None)
                .unwrap()
                .starts_with("# Global memory index")
        );
        assert!(
            storage
                .read_file(&storage.workspace_memory_file(), None, None)
                .unwrap()
                .starts_with("# Workspace memory index")
        );
        assert_eq!(
            storage
                .read_file(&legacy_root.join("MEMORY.md"), None, None)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(
            storage
                .list_memory_files()
                .unwrap()
                .iter()
                .all(|path| path.starts_with(&v2_root))
        );
        assert!(storage.clear_global().unwrap());
        assert!(
            storage
                .read_file(&storage.workspace_memory_file(), None, None)
                .unwrap()
                .starts_with("# Workspace memory index"),
            "clearing the sibling global scope must not make workspace memory unreadable"
        );
    }
}
