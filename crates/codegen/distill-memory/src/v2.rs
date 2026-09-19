// Modified for Distill by Samuel Fajreldines, 2026.
//! Isolated on-disk foundation for the v2 memory pipeline.
//!
//! V2 never reads from or writes to the legacy `memory/` tree. Each global or
//! workspace scope owns its topics, immutable observation inbox, archive,
//! generated manifest, durable state database, and lexical index.

use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use rusqlite::params;
use distill_tools::util::truncate_str;
use distill_sqlite_journal::JournalMode;

const STATE_SCHEMA_VERSION: &str = "1";
pub(crate) const MAX_DISCOVERED_FILES: usize = 10_000;
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 100_000;
const MAX_MANIFEST_BYTES: usize = 16 * 1024;
const MAX_MANIFEST_ENTRIES: usize = 512;
const MAX_DESCRIPTION_BYTES: usize = 512;
const MAX_SOURCE_BYTES: u64 = 64 * 1024;
/// Maximum size accepted for a user-authored remember observation.
///
/// This matches the per-source read budget so one manually saved observation
/// cannot persist more content than v2 will inspect while building a manifest.
pub const MAX_MANUAL_OBSERVATION_BYTES: usize = MAX_SOURCE_BYTES as usize;
const MAX_HASH_VERIFIED_MANIFEST_FILES: usize = 4_096;
/// Committed observation files not yet archived by Dream. The `REPLACE` join must
/// keep this exact text so the expression index on `consolidation_archives` applies.
pub(crate) const COMMITTED_UNARCHIVED_FILES_SQL: &str = "SELECT f.path, f.content_hash
     FROM capture_observation_files f
     JOIN capture_outcomes o USING(job_id)
     LEFT JOIN consolidation_archives a
       ON REPLACE(a.source_path, char(92), '/')
        = REPLACE(f.path, char(92), '/')
     WHERE a.source_path IS NULL
     ORDER BY f.path
     LIMIT ?1";

pub type Result<T> = std::result::Result<T, V2StorageError>;

#[derive(Debug, thiserror::Error)]
pub enum V2StorageError {
    #[error("failed to {operation} at {path}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to initialize {database} database at {path}")]
    Database {
        database: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("v2 memory directory {path} exceeds the {limit}-entry safety limit")]
    TooManyEntries { path: PathBuf, limit: usize },
    #[error(
        "v2 memory scope {path} is on a network filesystem; memory-v2 requires local shared-memory coordination"
    )]
    UnsupportedNetworkFilesystem { path: PathBuf },
    #[error("memory observation is {actual_bytes} bytes, exceeding the {limit_bytes}-byte limit")]
    ObservationTooLarge {
        actual_bytes: usize,
        limit_bytes: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V2MemoryScope {
    Global,
    Workspace,
}

impl V2MemoryScope {
    fn heading(self) -> &'static str {
        match self {
            Self::Global => "Global memory index",
            Self::Workspace => "Workspace memory index",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V2ManifestBudget {
    pub max_bytes: usize,
    pub max_entries: usize,
    pub max_description_bytes: usize,
}

impl Default for V2ManifestBudget {
    fn default() -> Self {
        Self {
            max_bytes: 8 * 1024,
            max_entries: 64,
            max_description_bytes: MAX_DESCRIPTION_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Manifest {
    pub content: String,
    pub discovered_entries: usize,
    pub included_entries: usize,
    pub is_truncated: bool,
}

/// Create one v2 scope without consulting any legacy path.
///
/// # Errors
///
/// Returns [`V2StorageError::Io`] when directories or the initial manifest
/// cannot be created, and [`V2StorageError::Database`] when either SQLite
/// database cannot be initialized.
pub fn ensure_scope_initialized(
    storage_root: &Path,
    scope_dir: &Path,
    scope: V2MemoryScope,
) -> Result<()> {
    ensure_scope_initialized_with_journal_mode(storage_root, scope_dir, scope, None)
}

pub fn ensure_scope_initialized_with_journal_mode(
    storage_root: &Path,
    scope_dir: &Path,
    scope: V2MemoryScope,
    journal_mode: Option<JournalMode>,
) -> Result<()> {
    reject_symlink_components(storage_root, scope_dir)?;
    for relative in ["topics", "observations", "observations/_inbox", "archive"] {
        let path = scope_dir.join(relative);
        reject_symlink(&path, "initialize symlinked v2 memory directory")?;
        std::fs::create_dir_all(&path).map_err(|source| V2StorageError::Io {
            operation: "create v2 memory directory",
            path,
            source,
        })?;
    }
    ensure_canonical_descendant(
        storage_root,
        scope_dir,
        "validate v2 memory scope containment",
    )?;

    let state_path = scope_dir.join("memory_state.sqlite");
    let journal_mode = journal_mode.unwrap_or_else(|| JournalMode::for_db_path(&state_path));
    if journal_mode == JournalMode::Truncate || distill_sqlite_journal::is_network_fs(scope_dir) {
        return Err(V2StorageError::UnsupportedNetworkFilesystem {
            path: scope_dir.to_path_buf(),
        });
    }
    reject_symlink_components(storage_root, scope_dir)?;
    reject_symlink(&state_path, "initialize symlinked v2 state database")?;
    initialize_state_db(&state_path)?;
    let index_path = scope_dir.join("index.sqlite");
    reject_symlink_components(storage_root, scope_dir)?;
    reject_symlink(&index_path, "initialize symlinked v2 lexical index")?;
    initialize_lexical_index(&index_path)?;

    let manifest_path = scope_dir.join("MEMORY.md");
    reject_symlink_components(storage_root, scope_dir)?;
    reject_symlink(&manifest_path, "initialize symlinked v2 manifest")?;
    regenerate_scope_manifest(scope_dir, scope, V2ManifestBudget::default())?;
    Ok(())
}

/// Atomically publish one immutable observation into a v2 scope inbox.
///
/// The caller must initialize the scope first. The next scope initialization
/// regenerates `MEMORY.md`; direct browse sees the observation immediately.
///
/// # Errors
///
/// Returns [`V2StorageError::ObservationTooLarge`] when `content` exceeds
/// [`MAX_MANUAL_OBSERVATION_BYTES`], or [`V2StorageError::Io`] if the inbox is
/// missing or unsafe or the observation cannot be durably published.
pub(crate) fn persist_observation(scope_dir: &Path, content: &str) -> Result<PathBuf> {
    if content.len() > MAX_MANUAL_OBSERVATION_BYTES {
        return Err(V2StorageError::ObservationTooLarge {
            actual_bytes: content.len(),
            limit_bytes: MAX_MANUAL_OBSERVATION_BYTES,
        });
    }

    let inbox = scope_dir.join("observations/_inbox");
    reject_symlink_components(scope_dir, &inbox)?;
    ensure_canonical_descendant(
        scope_dir,
        &inbox,
        "validate v2 observation inbox containment",
    )?;

    let mut temporary = tempfile::Builder::new()
        .prefix(".remember-")
        .suffix(".tmp")
        .tempfile_in(&inbox)
        .map_err(|source| V2StorageError::Io {
            operation: "create temporary v2 observation",
            path: inbox.clone(),
            source,
        })?;
    temporary
        .write_all(content.as_bytes())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| V2StorageError::Io {
            operation: "write temporary v2 observation",
            path: temporary.path().to_path_buf(),
            source,
        })?;

    let temporary_name = temporary
        .path()
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| V2StorageError::Io {
            operation: "derive v2 observation filename",
            path: temporary.path().to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "temporary observation filename is not valid UTF-8",
            ),
        })?;
    let observation_path = inbox.join(format!("{}.md", temporary_name.trim_start_matches('.')));
    temporary
        .persist_noclobber(&observation_path)
        .map_err(|error| V2StorageError::Io {
            operation: "publish v2 observation",
            path: observation_path.clone(),
            source: error.error,
        })?;
    Ok(observation_path)
}

fn reject_symlink_components(storage_root: &Path, scope_dir: &Path) -> Result<()> {
    let relative = scope_dir
        .strip_prefix(storage_root)
        .map_err(|_| V2StorageError::Io {
            operation: "validate v2 memory scope root",
            path: scope_dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "scope is outside the configured v2 storage root",
            ),
        })?;
    reject_symlink(storage_root, "initialize symlinked v2 memory root")?;
    let mut current = storage_root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        reject_symlink(&current, "initialize symlinked v2 memory path")?;
    }
    Ok(())
}

fn reject_symlink(path: &Path, operation: &'static str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(V2StorageError::Io {
            operation,
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "symbolic links are not allowed",
            ),
        }),
        Ok(_) => Ok(()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(V2StorageError::Io {
            operation: "inspect v2 memory path",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn ensure_canonical_descendant(root: &Path, target: &Path, operation: &'static str) -> Result<()> {
    let canonical_root = dunce::canonicalize(root).map_err(|source| V2StorageError::Io {
        operation,
        path: root.to_path_buf(),
        source,
    })?;
    let canonical_target = dunce::canonicalize(target).map_err(|source| V2StorageError::Io {
        operation,
        path: target.to_path_buf(),
        source,
    })?;
    if canonical_target.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(V2StorageError::Io {
            operation,
            path: target.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path escapes the configured v2 storage root",
            ),
        })
    }
}

/// Rebuild a scope's generated `MEMORY.md` with an atomic replacement.
///
/// # Errors
///
/// Returns [`V2StorageError::Io`] if source files cannot be enumerated or the
/// replacement cannot be persisted.
pub fn regenerate_scope_manifest(
    scope_dir: &Path,
    scope: V2MemoryScope,
    budget: V2ManifestBudget,
) -> Result<V2Manifest> {
    let manifest = render_scope_manifest(scope_dir, scope, budget)?;
    persist_scope_manifest(scope_dir, &manifest)?;
    Ok(manifest)
}

pub(crate) fn bump_manifest_revision(scope_dir: &Path) -> Result<()> {
    let path = scope_dir.join("memory_state.sqlite");
    let mut connection = JournalMode::for_db_path(&path)
        .open(&path)
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: path.clone(),
            source,
        })?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: path.clone(),
            source,
        })?;
    bump_manifest_revision_in_transaction(scope_dir, &transaction)?;
    transaction
        .commit()
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path,
            source,
        })
}

pub(crate) fn bump_manifest_revision_in_transaction(
    scope_dir: &Path,
    transaction: &rusqlite::Transaction<'_>,
) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO meta(key, value) VALUES ('capture_revision', '1')
             ON CONFLICT(key) DO UPDATE
             SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            [],
        )
        .map(|_| ())
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: scope_dir.join("memory_state.sqlite"),
            source,
        })
}

pub(crate) fn persist_scope_manifest(scope_dir: &Path, manifest: &V2Manifest) -> Result<()> {
    persist_replacing_file(&scope_dir.join("MEMORY.md"), manifest.content.as_bytes())
}

/// Render a deterministic, bounded pointer index for one memory scope.
///
/// # Errors
///
/// Returns [`V2StorageError::Io`] if a source directory cannot be enumerated.
pub fn render_scope_manifest(
    scope_dir: &Path,
    scope: V2MemoryScope,
    budget: V2ManifestBudget,
) -> Result<V2Manifest> {
    reject_symlink(scope_dir, "render symlinked v2 memory scope")?;
    let budget = V2ManifestBudget {
        max_bytes: budget.max_bytes.min(MAX_MANIFEST_BYTES),
        max_entries: budget.max_entries.min(MAX_MANIFEST_ENTRIES),
        max_description_bytes: budget.max_description_bytes.min(MAX_DESCRIPTION_BYTES),
    };
    let (mut entries, discovered_entries) =
        collect_entries(scope_dir, budget.max_description_bytes)?;
    // Observation files are written once, so modification time is when the
    // note entered the inbox; newest first, path as a deterministic tiebreak.
    entries.sort_by(|left, right| {
        left.kind.cmp(&right.kind).then_with(|| match left.kind {
            ManifestEntryKind::Topic => left.relative_path.cmp(&right.relative_path),
            ManifestEntryKind::Observation => right
                .modified
                .cmp(&left.modified)
                .then_with(|| right.relative_path.cmp(&left.relative_path)),
        })
    });

    let selected_entries = entries.into_iter().take(budget.max_entries);
    let header = manifest_header(scope, scope_dir);
    let mut content = truncate_str(&header, budget.max_bytes).to_owned();
    let was_header_limited = content.len() < header.len();
    let mut current_kind = None;
    let mut included_entries = 0;
    let mut was_byte_limited = was_header_limited;
    if !was_header_limited {
        for entry in selected_entries {
            let mut addition = String::new();
            if current_kind != Some(entry.kind) {
                addition.push_str(match entry.kind {
                    ManifestEntryKind::Topic => "\n## Topics\n",
                    ManifestEntryKind::Observation => "\n## Pending observations\n",
                });
            }
            addition.push_str("\n- **");
            addition.push_str(&entry.title);
            addition.push_str("**");
            if entry.kind == ManifestEntryKind::Topic && !entry.description.is_empty() {
                addition.push_str(" — ");
                addition.push_str(&entry.description);
            }
            addition.push_str(" (`");
            addition.push_str(&entry.relative_path);
            addition.push_str("`)\n");
            if content.len().saturating_add(addition.len()) > budget.max_bytes {
                was_byte_limited = true;
                // Observations are newest first, so once one does not fit the
                // remaining, older ones are dropped rather than back-filled.
                if entry.kind == ManifestEntryKind::Observation {
                    break;
                }
                continue;
            }
            content.push_str(&addition);
            current_kind = Some(entry.kind);
            included_entries += 1;
        }
    }

    if discovered_entries == 0 {
        let empty = format!("{header}\nNo memory files have been recorded yet.\n");
        content = truncate_str(&empty, budget.max_bytes).to_owned();
        was_byte_limited = content.len() < empty.len();
    }

    let was_entry_limited = discovered_entries > included_entries;

    Ok(V2Manifest {
        content,
        discovered_entries,
        included_entries,
        is_truncated: was_entry_limited || was_byte_limited,
    })
}

fn manifest_header(scope: V2MemoryScope, scope_dir: &Path) -> String {
    format!(
        "# {}\n\n> Generated by Grok. Do not edit this file directly.\n> Paths are relative to `{}`.\n",
        scope.heading(),
        sanitize_inline(&scope_dir.display().to_string())
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ManifestEntryKind {
    Topic,
    Observation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestEntry {
    kind: ManifestEntryKind,
    relative_path: String,
    title: String,
    description: String,
    modified: Option<std::time::SystemTime>,
}

fn collect_entries(
    scope_dir: &Path,
    max_description_bytes: usize,
) -> Result<(Vec<ManifestEntry>, usize)> {
    let mut entries = Vec::new();
    let excluded = excluded_manifest_paths(scope_dir)?;
    let mut discovered_entries = collect_entries_from(
        scope_dir,
        Path::new("topics"),
        ManifestEntryKind::Topic,
        max_description_bytes,
        &excluded,
        &mut entries,
    )?;
    discovered_entries += collect_entries_from(
        scope_dir,
        Path::new("observations/_inbox"),
        ManifestEntryKind::Observation,
        max_description_bytes,
        &excluded,
        &mut entries,
    )?;
    Ok((entries, discovered_entries))
}

fn collect_entries_from(
    scope_dir: &Path,
    relative_dir: &Path,
    kind: ManifestEntryKind,
    max_description_bytes: usize,
    tombstoned: &BTreeSet<String>,
    entries: &mut Vec<ManifestEntry>,
) -> Result<usize> {
    let directory = scope_dir.join(relative_dir);
    reject_symlink(&directory, "read symlinked v2 memory directory")?;
    if directory.exists() {
        ensure_canonical_descendant(
            scope_dir,
            &directory,
            "validate v2 manifest source containment",
        )?;
    }
    let read_dir = match std::fs::read_dir(&directory) {
        Ok(read_dir) => read_dir,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(V2StorageError::Io {
                operation: "enumerate v2 memory files",
                path: directory,
                source,
            });
        }
    };

    let remaining = MAX_DISCOVERED_FILES.saturating_sub(entries.len());
    let mut candidates = Vec::new();
    for (entry_index, entry) in read_dir.enumerate() {
        if entry_index >= MAX_DIRECTORY_ENTRIES {
            return Err(V2StorageError::TooManyEntries {
                path: directory,
                limit: MAX_DIRECTORY_ENTRIES,
            });
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(source) => {
                return Err(V2StorageError::Io {
                    operation: "read v2 memory directory entry",
                    path: directory.clone(),
                    source,
                });
            }
        };
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| V2StorageError::Io {
            operation: "inspect v2 memory entry",
            path: path.clone(),
            source,
        })?;
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }

        let relative_path = match path.strip_prefix(scope_dir) {
            Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if tombstoned.contains(&relative_path) {
            continue;
        }
        let modified = entry.metadata().and_then(|meta| meta.modified()).ok();
        candidates.push((relative_path, path, modified));
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));

    let discovered_entries = candidates.len();
    for (relative_path, path, modified) in candidates.into_iter().take(remaining) {
        let (title, description) =
            summarize_file(&path, max_description_bytes).unwrap_or_else(|_| {
                let title = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("memory")
                    .to_owned();
                (title, String::new())
            });
        entries.push(ManifestEntry {
            kind,
            relative_path: sanitize_inline(&relative_path),
            title: sanitize_inline(truncate_str(&title, MAX_DESCRIPTION_BYTES)),
            description: sanitize_inline(&description),
            modified,
        });
    }
    Ok(discovered_entries)
}

pub(crate) fn excluded_manifest_paths(scope_dir: &Path) -> Result<BTreeSet<String>> {
    excluded_manifest_paths_with_journal_mode(scope_dir, None)
}

pub(crate) fn excluded_manifest_paths_with_journal_mode(
    scope_dir: &Path,
    journal_mode: Option<JournalMode>,
) -> Result<BTreeSet<String>> {
    let state_path = scope_dir.join("memory_state.sqlite");
    let journal_mode = journal_mode.unwrap_or_else(|| JournalMode::for_db_path(&state_path));
    match std::fs::symlink_metadata(journal_mode.effective_db_path(&state_path)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeSet::new());
        }
        Err(source) => {
            return Err(V2StorageError::Io {
                operation: "inspect v2 state exclusion ledger",
                path: state_path,
                source,
            });
        }
        Ok(_) => {}
    }
    let connection =
        journal_mode
            .open_readonly(&state_path)
            .map_err(|source| V2StorageError::Database {
                database: "state",
                path: state_path.clone(),
                source,
            })?;
    let mut exclusion_tables = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table'
               AND name IN (
                   'memory_v2_tombstones',
                   'memory_v2_hidden_observations',
                   'memory_v2_quarantined_paths'
               )",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<BTreeSet<_>, _>>()
        })
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: state_path.clone(),
            source,
        })?;
    let state_schema_version = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: state_path.clone(),
            source,
        })?;
    if state_schema_version == "4" && exclusion_tables.len() != 3 {
        return Err(V2StorageError::Database {
            database: "state",
            path: state_path,
            source: rusqlite::Error::InvalidQuery,
        });
    }
    let mut excluded = BTreeSet::new();
    for (table, query) in [
        (
            "memory_v2_tombstones",
            "SELECT relative_path FROM memory_v2_tombstones ORDER BY relative_path",
        ),
        (
            "memory_v2_hidden_observations",
            "SELECT relative_path FROM memory_v2_hidden_observations ORDER BY relative_path",
        ),
        (
            "memory_v2_quarantined_paths",
            "SELECT relative_path FROM memory_v2_quarantined_paths ORDER BY relative_path",
        ),
    ] {
        if !exclusion_tables.remove(table) {
            continue;
        }
        let mut statement =
            connection
                .prepare(query)
                .map_err(|source| V2StorageError::Database {
                    database: "state",
                    path: state_path.clone(),
                    source,
                })?;
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))
            .and_then(|rows| rows.collect::<std::result::Result<Vec<_>, _>>())
            .map_err(|source| V2StorageError::Database {
                database: "state",
                path: state_path.clone(),
                source,
            })?;
        excluded.extend(paths.into_iter().map(|path| path.replace('\\', "/")));
    }

    let capture_tables = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table'
               AND name IN (
                   'capture_observation_files',
                   'capture_outcomes',
                   'consolidation_archives'
               )",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<BTreeSet<_>, _>>()
        })
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: state_path.clone(),
            source,
        })?;
    if !capture_tables.contains("capture_observation_files")
        && capture_tables
            .iter()
            .all(|table| table == "consolidation_archives")
    {
        return Ok(excluded);
    }
    if capture_tables.len() != 3 {
        return Err(V2StorageError::Database {
            database: "state",
            path: state_path,
            source: rusqlite::Error::InvalidQuery,
        });
    }

    let mut statement = connection
        .prepare(COMMITTED_UNARCHIVED_FILES_SQL)
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: state_path.clone(),
            source,
        })?;
    let committed = statement
        .query_map(params![MAX_HASH_VERIFIED_MANIFEST_FILES + 1], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .and_then(|rows| rows.collect::<std::result::Result<Vec<_>, _>>())
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: state_path.clone(),
            source,
        })?;
    if committed.len() > MAX_HASH_VERIFIED_MANIFEST_FILES {
        return Err(V2StorageError::TooManyEntries {
            path: state_path,
            limit: MAX_HASH_VERIFIED_MANIFEST_FILES,
        });
    }
    for (relative, expected_hash) in committed {
        let relative = relative.replace('\\', "/");
        if !excluded.contains(&relative)
            && !committed_manifest_file_matches(scope_dir, &relative, &expected_hash)
        {
            excluded.insert(relative);
        }
    }
    Ok(excluded)
}

fn committed_manifest_file_matches(scope_dir: &Path, relative: &str, expected_hash: &str) -> bool {
    let relative = Path::new(relative);
    if relative.parent() != Some(Path::new("observations/_inbox"))
        || relative.extension().and_then(|value| value.to_str()) != Some("md")
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }
    let path = scope_dir.join(relative);
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return false;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let read = std::fs::File::open(&path).and_then(|mut file| {
        std::io::Read::by_ref(&mut file)
            .take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)
    });
    read.is_ok()
        && bytes.len() as u64 <= MAX_SOURCE_BYTES
        && blake3::hash(&bytes).to_hex().as_str() == expected_hash
}

pub(crate) fn is_durably_excluded(scope_dir: &Path, relative_path: &Path) -> Result<bool> {
    is_durably_excluded_with_journal_mode(scope_dir, relative_path, None)
}

pub(crate) fn is_durably_excluded_with_journal_mode(
    scope_dir: &Path,
    relative_path: &Path,
    journal_mode: Option<JournalMode>,
) -> Result<bool> {
    let state_path = scope_dir.join("memory_state.sqlite");
    let journal_mode = journal_mode.unwrap_or_else(|| JournalMode::for_db_path(&state_path));
    match std::fs::symlink_metadata(journal_mode.effective_db_path(&state_path)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(V2StorageError::Io {
                operation: "inspect v2 state exclusion ledger",
                path: state_path,
                source,
            });
        }
        Ok(_) => {}
    }
    let connection =
        journal_mode
            .open_readonly(&state_path)
            .map_err(|source| V2StorageError::Database {
                database: "state",
                path: state_path.clone(),
                source,
            })?;
    let relative_path = relative_path.to_string_lossy().replace('\\', "/");
    for (table, query) in [
        (
            "memory_v2_tombstones",
            "SELECT EXISTS(
                SELECT 1 FROM memory_v2_tombstones
                WHERE REPLACE(relative_path, char(92), '/') = ?1
             )",
        ),
        (
            "memory_v2_hidden_observations",
            "SELECT EXISTS(
                SELECT 1 FROM memory_v2_hidden_observations
                WHERE REPLACE(relative_path, char(92), '/') = ?1
             )",
        ),
        (
            "memory_v2_quarantined_paths",
            "SELECT EXISTS(
                SELECT 1 FROM memory_v2_quarantined_paths
                WHERE REPLACE(relative_path, char(92), '/') = ?1
             )",
        ),
    ] {
        let table_exists = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
                 )",
                params![table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|source| V2StorageError::Database {
                database: "state",
                path: state_path.clone(),
                source,
            })?;
        if table_exists
            && connection
                .query_row(query, params![relative_path], |row| row.get::<_, bool>(0))
                .map_err(|source| V2StorageError::Database {
                    database: "state",
                    path: state_path.clone(),
                    source,
                })?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn summarize_file(path: &Path, max_description_bytes: usize) -> std::io::Result<(String, String)> {
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_SOURCE_BYTES)
        .read_to_end(&mut bytes)?;
    let body = String::from_utf8_lossy(&bytes);
    let mut title = None;
    let mut description = None;

    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if title.is_none() && line.starts_with('#') {
            let heading = line.trim_start_matches('#').trim();
            if !heading.is_empty() {
                title = Some(heading.to_owned());
            }
            continue;
        }
        if description.is_none()
            && !line.starts_with('#')
            && !line.starts_with("<!--")
            && !line.starts_with('>')
        {
            description = Some(truncate_str(line, max_description_bytes).to_owned());
        }
        if title.is_some() && description.is_some() {
            break;
        }
    }

    let fallback_title = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("memory")
        .to_owned();
    Ok((
        title.unwrap_or(fallback_title),
        description.unwrap_or_default(),
    ))
}

fn sanitize_inline(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' => ' ',
            '`' => '\'',
            _ => character,
        })
        .collect()
}

fn initialize_state_db(path: &Path) -> Result<()> {
    let connection = JournalMode::for_db_path(path)
        .open(path)
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: path.to_path_buf(),
            source,
        })?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )
        .and_then(|_| {
            connection.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
                params![STATE_SCHEMA_VERSION],
            )
        })
        .map(|_| ())
        .map_err(|source| V2StorageError::Database {
            database: "state",
            path: path.to_path_buf(),
            source,
        })
}

fn initialize_lexical_index(path: &Path) -> Result<()> {
    let connection = JournalMode::for_db_path(path)
        .open(path)
        .map_err(|source| V2StorageError::Database {
            database: "lexical index",
            path: path.to_path_buf(),
            source,
        })?;
    connection
        .execute_batch(&crate::schema::schema_sql(1, false))
        .and_then(|_| {
            connection.execute(
                crate::schema::UPSERT_META_SQL,
                params!["retrieval_mode", "fts_only"],
            )
        })
        .map(|_| ())
        .map_err(|source| V2StorageError::Database {
            database: "lexical index",
            path: path.to_path_buf(),
            source,
        })
}

fn persist_replacing_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| V2StorageError::Io {
        operation: "resolve v2 manifest parent",
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent"),
    })?;
    std::fs::create_dir_all(parent).map_err(|source| V2StorageError::Io {
        operation: "create v2 manifest parent",
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| V2StorageError::Io {
            operation: "create temporary v2 manifest",
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(contents)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| V2StorageError::Io {
            operation: "write temporary v2 manifest",
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| V2StorageError::Io {
            operation: "publish v2 manifest",
            path: path.to_path_buf(),
            source: error.error,
        })
}

#[cfg(test)]
#[path = "v2_tests.rs"]
mod tests;
