// Modified for Distill by Samuel Fajreldines, 2026.
//! One-shot carry-over of a legacy curated `MEMORY.md` into v2 topic files.
//!
//! Legacy Dream wrote one self-contained `##` section per topic, so each
//! section becomes a v2 topic. The legacy tree is only read, never modified.
//! The source hash is recorded in the scope's `meta` table so a session start
//! is a no-op until a legacy client rewrites the file again.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension as _, params};
use distill_tools::util::truncate_str;
use distill_sqlite_journal::JournalMode;

use crate::storage::slugify;
use crate::v2::V2MemoryScope;
use crate::v2_access::{MAX_TOPIC_FILE_BYTES, V2AccessError, V2MemoryAccessPolicy};
use crate::v2_clock::V2Clock;

const META_HASH_KEY: &str = "legacy_carryover_hash";
const META_AT_KEY: &str = "legacy_carryover_at";
/// Legacy Dream output was capped at 16k chars; anything far beyond that is
/// not a curated file and is not worth flooding the index with.
const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
/// Sections beyond this many are folded into one topic so the carried-over
/// notes cannot consume the 64-entry manifest budget by themselves.
const MAX_SEPARATE_SECTIONS: usize = 24;
const MAX_DESCRIPTION_CHARS: usize = 160;
const MAX_SLUG_CHARS: usize = 60;
const FOLDED_TITLE: &str = "More notes";
const APPENDED_HEADING: &str = "## From earlier sessions";

pub type Result<T> = std::result::Result<T, V2CarryoverError>;

#[derive(Debug, thiserror::Error)]
pub enum V2CarryoverError {
    #[error("failed to read legacy memory file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "legacy memory file {path} is {actual_bytes} bytes, above the {limit_bytes}-byte limit"
    )]
    SourceTooLarge {
        path: PathBuf,
        actual_bytes: u64,
        limit_bytes: u64,
    },
    #[error("v2 state database error during legacy carry-over")]
    Database(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct V2CarryoverReport {
    pub topics_created: u32,
    pub topics_appended: u32,
    pub sections_skipped: u32,
    pub bytes_written: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V2CarryoverOutcome {
    /// No legacy file exists for this scope.
    MissingSource,
    /// The legacy file is the untouched scaffold or empty.
    NothingToCarry,
    /// The legacy file was already carried over and has not changed since.
    Unchanged,
    Imported(V2CarryoverReport),
}

/// `~/.grok/memory/`, the root the legacy pipeline writes under.
pub fn default_legacy_memory_root() -> PathBuf {
    distill_tools::util::distill_home::distill_home().join("memory")
}

/// Path of the legacy curated file that corresponds to a v2 scope.
///
/// Both pipelines derive the workspace directory name the same way, so the v2
/// workspace directory name is the legacy one.
pub fn legacy_memory_file(
    legacy_root: &Path,
    scope: V2MemoryScope,
    workspace_dir_name: &str,
) -> PathBuf {
    match scope {
        V2MemoryScope::Global => legacy_root.join("MEMORY.md"),
        V2MemoryScope::Workspace => legacy_root.join(workspace_dir_name).join("MEMORY.md"),
    }
}

/// Carry the legacy file at `source` into `scope_dir/topics/`.
///
/// Writes go through `access` so tombstoned names stay excluded, the manifest
/// revision is bumped, and the model-visible index is regenerated. Per-section
/// write failures are counted as skipped; only source and database failures
/// abort.
///
/// # Errors
///
/// Returns [`V2CarryoverError::Read`] for source I/O other than not-found,
/// [`V2CarryoverError::SourceTooLarge`] above [`MAX_SOURCE_BYTES`], and
/// [`V2CarryoverError::Database`] when the state database cannot be updated.
pub fn carry_over_legacy_memory(
    scope_dir: &Path,
    source: &Path,
    access: &V2MemoryAccessPolicy,
    clock: &dyn V2Clock,
) -> Result<V2CarryoverOutcome> {
    let Some(content) = read_source(source)? else {
        return Ok(V2CarryoverOutcome::MissingSource);
    };
    // Scaffold headings and `<!-- … -->` placeholders yield no topics, so an
    // untouched or nearly untouched file is detected by content, not by size.
    let topics = split_into_topics(&content);
    if topics.is_empty() {
        return Ok(V2CarryoverOutcome::NothingToCarry);
    }
    let source_hash = blake3::hash(content.as_bytes()).to_hex().to_string();

    let state_path = scope_dir.join("memory_state.sqlite");
    let mut connection = JournalMode::for_db_path(&state_path).open(&state_path)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let recorded_hash = transaction
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![META_HASH_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if recorded_hash.as_deref() == Some(source_hash.as_str()) {
        return Ok(V2CarryoverOutcome::Unchanged);
    }

    let topics_dir = scope_dir.join("topics");
    let mut report = V2CarryoverReport::default();
    let mut had_write_error = false;
    for topic in topics {
        let path = topics_dir.join(format!("{}.md", topic.slug));
        match write_topic(&path, &topic, access, &transaction) {
            Ok(TopicWrite::Created(bytes)) => {
                report.topics_created += 1;
                report.bytes_written += bytes;
            }
            Ok(TopicWrite::Appended(bytes)) => {
                report.topics_appended += 1;
                report.bytes_written += bytes;
            }
            Ok(TopicWrite::Skipped) => report.sections_skipped += 1,
            Err(error) => {
                had_write_error = true;
                report.sections_skipped += 1;
                tracing::warn!(
                    target: crate::MEMORY_LOG_TARGET,
                    path = %path.display(),
                    error = %error,
                    "MEMORY_CARRYOVER: could not write one legacy section; will retry next start"
                );
            }
        }
    }

    // Successful writes are kept, but the hash is recorded only when every
    // section either landed or was skipped for a deterministic reason, so a
    // transient write failure is retried on the next start rather than lost.
    if !had_write_error {
        for (key, value) in [
            (META_HASH_KEY, source_hash),
            (META_AT_KEY, clock.now_unix_seconds().to_string()),
        ] {
            transaction.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
    }
    transaction.commit()?;
    Ok(V2CarryoverOutcome::Imported(report))
}

fn read_source(source: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(V2CarryoverError::Read {
                path: source.to_path_buf(),
                source: error,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Ok(None);
    }
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(V2CarryoverError::SourceTooLarge {
            path: source.to_path_buf(),
            actual_bytes: metadata.len(),
            limit_bytes: MAX_SOURCE_BYTES,
        });
    }
    let bytes = read_bounded(source, MAX_SOURCE_BYTES).map_err(|error| V2CarryoverError::Read {
        path: source.to_path_buf(),
        source: error,
    })?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(V2CarryoverError::SourceTooLarge {
            path: source.to_path_buf(),
            actual_bytes: bytes.len() as u64,
            limit_bytes: MAX_SOURCE_BYTES,
        });
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Read at most `limit + 1` bytes so callers can detect an over-limit file
/// without trusting a size observed before the open.
fn read_bounded(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

enum TopicWrite {
    Created(u64),
    Appended(u64),
    /// Deterministically not written: the name is tombstoned, the existing
    /// topic already holds every paragraph, or the size cap would be exceeded.
    /// Counts as done so the source hash can be recorded.
    Skipped,
}

fn write_topic(
    path: &Path,
    topic: &CarriedTopic,
    access: &V2MemoryAccessPolicy,
    transaction: &rusqlite::Transaction<'_>,
) -> std::result::Result<TopicWrite, V2AccessError> {
    let existing = match read_bounded(path, MAX_TOPIC_FILE_BYTES) {
        Ok(existing) if existing.len() as u64 > MAX_TOPIC_FILE_BYTES => {
            return Ok(TopicWrite::Skipped);
        }
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let rendered = topic.render();
            return match access.write_file_typed_in_transaction(
                path,
                rendered.as_bytes(),
                transaction,
            ) {
                Ok(_) => Ok(TopicWrite::Created(rendered.len() as u64)),
                Err(V2AccessError::Excluded(_)) => Ok(TopicWrite::Skipped),
                Err(error) => Err(error),
            };
        }
        Err(source) => {
            return Err(V2AccessError::Inspect {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let existing_text = String::from_utf8_lossy(&existing);
    // Whole-paragraph dedupe so a legacy rewrite that only adds a paragraph
    // appends that paragraph, not the whole section again. Appended paragraphs
    // were heading-shifted, so compare both forms.
    let existing_paragraphs: HashSet<&str> = existing_text.split("\n\n").map(str::trim).collect();
    let new_paragraphs: Vec<&str> = topic
        .body
        .split("\n\n")
        .map(str::trim)
        .filter(|paragraph| {
            !paragraph.is_empty()
                && !existing_paragraphs.contains(paragraph)
                && !existing_paragraphs.contains(shift_headings(paragraph, 1).trim())
        })
        .collect();
    if new_paragraphs.is_empty() {
        return Ok(TopicWrite::Skipped);
    }
    let merged = format!(
        "{}\n\n{APPENDED_HEADING}\n\n{}\n",
        existing_text.trim_end(),
        shift_headings(&new_paragraphs.join("\n\n"), 1)
    );
    if merged.len() as u64 > MAX_TOPIC_FILE_BYTES {
        return Ok(TopicWrite::Skipped);
    }
    match access
        .record_read_typed(path, &existing)
        .and_then(|_| access.write_file_typed_in_transaction(path, merged.as_bytes(), transaction))
    {
        Ok(_) => Ok(TopicWrite::Appended(merged.len() as u64)),
        Err(V2AccessError::Excluded(_)) => Ok(TopicWrite::Skipped),
        Err(error) => Err(error),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CarriedTopic {
    slug: String,
    title: String,
    body: String,
}

impl CarriedTopic {
    fn render(&self) -> String {
        let description = first_sentence(&self.body);
        let mut body = self.body.trim().to_owned();
        let header_len = self.title.len() + description.len() + 8;
        let limit = (MAX_TOPIC_FILE_BYTES as usize).saturating_sub(header_len);
        if body.len() > limit {
            let mut cut = limit;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(body[..cut].rfind("\n\n").unwrap_or(cut));
        }
        if description.is_empty() {
            format!("# {}\n\n{body}\n", self.title)
        } else {
            format!("# {}\n{description}\n\n{body}\n", self.title)
        }
    }
}

/// Split a legacy `MEMORY.md` into topics at the shallowest heading level
/// after the title (`#` for hand-curated outlines, `##` for Dream output),
/// or the whole file when it has no headings. Duplicate slugs within one file
/// merge; sections past [`MAX_SEPARATE_SECTIONS`] fold into one topic.
fn split_into_topics(content: &str) -> Vec<CarriedTopic> {
    let (title, remainder) = strip_title_and_scaffold(content);
    let lines: Vec<Line<'_>> = markdown_lines(remainder).collect();
    let split_level = [1, 2]
        .into_iter()
        .find(|level| lines.iter().any(|line| line.heading_level == Some(*level)));
    let sections = match split_level {
        Some(level) => split_at_level(&lines, level),
        None => vec![Section {
            title: title.unwrap_or_else(|| "Notes".to_owned()),
            body: remainder.trim().to_owned(),
        }],
    };

    let mut topics: Vec<CarriedTopic> = Vec::new();
    for section in sections {
        let body = section.body.trim();
        if is_placeholder_only(body) {
            continue;
        }
        let slug = slug_for(&section.title);
        match topics.iter_mut().find(|topic| topic.slug == slug) {
            Some(existing) => {
                existing.body.push_str("\n\n");
                existing.body.push_str(body);
            }
            None => topics.push(CarriedTopic {
                slug,
                title: section.title,
                body: body.to_owned(),
            }),
        }
    }
    if topics.len() > MAX_SEPARATE_SECTIONS {
        let folded = topics.split_off(MAX_SEPARATE_SECTIONS);
        let body = folded
            .iter()
            .map(|topic| format!("## {}\n\n{}", topic.title, shift_headings(&topic.body, 1)))
            .collect::<Vec<_>>()
            .join("\n\n");
        topics.push(CarriedTopic {
            slug: slug_for(FOLDED_TITLE),
            title: FOLDED_TITLE.to_owned(),
            body,
        });
    }
    topics
}

/// True for an empty body or one made only of scaffold `<!-- … -->` lines.
fn is_placeholder_only(body: &str) -> bool {
    body.lines()
        .map(str::trim)
        .all(|line| line.is_empty() || (line.starts_with("<!--") && line.ends_with("-->")))
}

/// Drop the legacy `# Global Memory` / `# Project Memory — …` title and the
/// scaffold blockquote that follows it; return the title and what remains.
fn strip_title_and_scaffold(content: &str) -> (Option<String>, &str) {
    let mut title = None;
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        let is_title = title.is_none() && trimmed.starts_with("# ");
        if !(trimmed.is_empty() || trimmed.starts_with('>') || is_title) {
            break;
        }
        if is_title {
            title = Some(trimmed[2..].trim().to_owned());
        }
        offset += line.len();
    }
    (title, &content[offset..])
}

#[derive(Debug, Clone, Copy)]
struct Line<'a> {
    text: &'a str,
    /// True for fence markers and everything between them.
    in_fence: bool,
    /// `None` inside fenced code blocks and for non-heading lines.
    heading_level: Option<usize>,
}

fn markdown_lines(content: &str) -> impl Iterator<Item = Line<'_>> {
    let mut inside = false;
    content.split_inclusive('\n').map(move |text| {
        let trimmed = text.trim_end_matches(['\r', '\n']);
        let is_marker = trimmed.trim_start().starts_with("```");
        if is_marker {
            inside = !inside;
        }
        let in_fence = inside || is_marker;
        let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
        let heading_level =
            (!in_fence && hashes > 0 && trimmed[hashes..].starts_with(' ')).then_some(hashes);
        Line {
            text,
            in_fence,
            heading_level,
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Section {
    title: String,
    body: String,
}

fn split_at_level(lines: &[Line<'_>], level: usize) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut preamble = String::new();
    let mut current: Option<Section> = None;
    for line in lines {
        if line.heading_level == Some(level) {
            sections.extend(current.take());
            current = Some(Section {
                title: line.text[level..].trim().to_owned(),
                body: String::new(),
            });
        } else {
            match current.as_mut() {
                Some(section) => section.body.push_str(line.text),
                None => preamble.push_str(line.text),
            }
        }
    }
    sections.extend(current);
    // Sub-headings of a `##` section start at `###`; promote them so the new
    // topic's sections are `##` like Dream-written topics.
    let promotion = 1 - level as isize;
    for section in &mut sections {
        section.body = shift_headings(&section.body, promotion).trim().to_owned();
    }
    if !preamble.trim().is_empty() {
        sections.insert(
            0,
            Section {
                title: "Overview".to_owned(),
                body: preamble.trim().to_owned(),
            },
        );
    }
    sections
}

/// Shift heading levels by `delta` outside fenced code blocks, never below
/// level 2 so a topic body has no second `#` title.
fn shift_headings(content: &str, delta: isize) -> String {
    let mut out = String::with_capacity(content.len());
    for line in markdown_lines(content) {
        match line.heading_level {
            Some(level) => {
                let new_level = (level as isize + delta).max(2) as usize;
                out.push_str(&"#".repeat(new_level));
                out.push_str(&line.text[level..]);
            }
            None => out.push_str(line.text),
        }
    }
    out
}

fn slug_for(title: &str) -> String {
    let slug = slugify(title, MAX_SLUG_CHARS);
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "notes".to_owned()
    } else {
        slug.to_owned()
    }
}

fn first_sentence(body: &str) -> String {
    let Some(line) = markdown_lines(body)
        .filter(|line| !line.in_fence && line.heading_level.is_none())
        .map(|line| line.text.trim())
        .find(|text| !text.is_empty() && !text.starts_with("<!--") && !text.starts_with('>'))
    else {
        return String::new();
    };
    let line = line.trim_start_matches(['-', '*', ' ']).trim();
    let end = line.find(". ").map_or(line.len(), |index| index + 1);
    truncate_str(&line[..end], MAX_DESCRIPTION_CHARS)
        .trim()
        .to_owned()
}

#[cfg(test)]
#[path = "v2_carryover_tests.rs"]
mod tests;
