// Modified for Distill by Samuel Fajreldines, 2026.
//! autoDream gating and execution logic.
//!
//! Decides whether a dream consolidation should fire based on config gates, time elapsed, and session count.

use std::path::Path;
use std::time::SystemTime;

use super::dream_lock::DreamLock;
use distill_config_types::MemoryDreamConfig;

/// Result of the dream gate check.
#[derive(Debug, PartialEq, Eq)]
pub enum DreamGate {
    /// All gates passed; the dream should fire.
    Open {
        /// Sessions to consolidate (file stems for the prompt).
        sessions: Vec<String>,
    },
    /// Dream is disabled by config.
    Disabled,
    /// Not enough time has passed since last consolidation.
    TooSoon { hours_since: u64 },
    /// Not enough sessions since last consolidation.
    TooFewSessions { count: usize, required: u64 },
    /// Lock acquisition or I/O failed.
    Error(String),
}

/// Check all dream gates, cheapest first: `dream.enabled`, then elapsed hours against `min_hours`, then session count against `min_sessions`.
/// Does not acquire the lock; callers acquire it after confirming `Open`.
pub fn check_dream_gates(
    config: &MemoryDreamConfig,
    lock: &DreamLock,
    sessions_dir: &Path,
    current_session_sid8: Option<&str>,
) -> DreamGate {
    if !config.enabled {
        return DreamGate::Disabled;
    }

    // Time gate: enough hours must have passed since the last consolidation
    let last_at = match lock.last_consolidated_at() {
        Ok(Some(t)) => t,
        Ok(None) => SystemTime::UNIX_EPOCH,
        Err(e) => return DreamGate::Error(e.to_string()),
    };
    let elapsed = SystemTime::now()
        .duration_since(last_at)
        .unwrap_or_default();
    let hours_since = elapsed.as_secs() / 3600;
    if hours_since < config.min_hours {
        return DreamGate::TooSoon { hours_since };
    }

    // Session gate: enough sessions must have accumulated since the last consolidation
    let sessions =
        match super::dream_lock::sessions_since(sessions_dir, last_at, current_session_sid8) {
            Ok(s) => s,
            Err(e) => return DreamGate::Error(e.to_string()),
        };
    if (sessions.len() as u64) < config.min_sessions {
        return DreamGate::TooFewSessions {
            count: sessions.len(),
            required: config.min_sessions,
        };
    }

    DreamGate::Open { sessions }
}

// ---------------------------------------------------------------------------
// Dream prompt, response processing, and execution
// ---------------------------------------------------------------------------

use super::text_utils::{has_markdown_headers, is_no_reply};

const LOG: &str = "distill_memory";

pub const DREAM_SYSTEM_PROMPT: &str = "\
You are performing a dream \u{2014} a reflective pass over memory files. \
Synthesize recent session logs into durable, well-organized memories \
so future sessions orient quickly.

You will receive the contents of recent session logs. \
You may also receive an existing memory document \u{2014} merge it with new sessions \
rather than discarding prior knowledge. Your job:

1. **Merge** related information into coherent topic summaries
2. **Resolve** contradictions \u{2014} if a recent session disproves an older fact, keep only the current truth
3. **Convert** relative dates (\"yesterday\", \"last week\") to absolute dates
4. **Discard** ephemeral details:
   - Greetings, meta-commentary, tool output noise
   - Message counts and tool-usage statistics
   - 'Current state' and 'Next steps' sections
   - User preferences already in global memory (OS, shell, editor)
   - Session metadata (dates, message counts)
5. **Preserve** decisions, rationale, architecture, preferences, and problem/solution pairs
6. **Generalize** task narratives into reusable mechanisms, rules, and root causes
7. **Ground details** — use stable repo-relative paths for project structure; include an absolute \
workspace root only when operationally necessary and present in the session logs. Copy exact \
identifiers and numerical values only when they appear verbatim in the input; omit unsupported details

Respond with a single markdown document. Use ## headers to separate topics. \
Each topic should be self-contained and useful to a future session that knows \
nothing about the current conversation.

If the session logs contain nothing worth persisting, respond with NO_REPLY.";

/// Outcome of a dream consolidation attempt.
#[derive(Debug)]
pub struct DreamResult {
    pub status: DreamStatus,
    /// Number of gate-eligible sessions: all sessions passed to the dream, including those beyond the 32K input cap that were never read.
    pub sessions_eligible: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DreamStatus {
    /// Dream ran and produced consolidated output.
    Completed { chars_written: usize },
    /// Dream ran but model returned nothing useful.
    NothingToConsolidate,
    /// Dream failed.
    Failed(String),
}

const MAX_DREAM_INPUT_CHARS: usize = 32_000;

/// Output of [`build_dream_user_message`]: the prompt text and the stems that were actually read (within the size cap).
#[derive(Debug)]
pub struct DreamMessage {
    /// The concatenated session content for the model prompt.
    pub content: String,
    /// File stems that were successfully read and included; only these sessions are cleaned up after a successful consolidation.
    /// Stems beyond the [`MAX_DREAM_INPUT_CHARS`] cap are excluded so their content survives for a future dream pass.
    pub processed_stems: Vec<String>,
}

/// Returns `true` if the content is scaffold boilerplate that should not be fed to the dream model as existing memory context.
/// A file is scaffold only if it is short (under 500 bytes trimmed) and contains a scaffold marker.
/// Files with substantial content are never scaffold, even if they contain leftover marker strings from the initial template.
pub(crate) fn is_scaffold_template(content: &str) -> bool {
    const SCAFFOLD_MAX_LEN: usize = 500;
    const MARKERS: &[&str] = &[
        "Auto-populated by dream consolidation",
        "Add project-specific knowledge here",
        "Add any cross-project preferences here",
    ];
    let trimmed = content.trim();
    trimmed.len() < SCAFFOLD_MAX_LEN && MARKERS.iter().any(|marker| trimmed.contains(marker))
}

/// Build the user message for the dream model call from session log contents.
/// Non-scaffold `existing_memory` is prepended before the session logs so the model merges prior knowledge instead of discarding it.
pub fn build_dream_user_message(
    sessions_dir: &Path,
    stems: &[String],
    existing_memory: Option<&str>,
) -> Option<DreamMessage> {
    let existing_len = existing_memory.map_or(0, str::len);
    let mut buf = String::with_capacity(existing_len + stems.len().min(10) * 2000);

    if let Some(mem) = existing_memory {
        let trimmed = mem.trim();
        if !trimmed.is_empty() && !is_scaffold_template(trimmed) {
            buf.push_str("--- Existing Memory (merge with new sessions) ---\n\n");
            let cap = MAX_DREAM_INPUT_CHARS / 2;
            if trimmed.len() <= cap {
                buf.push_str(trimmed);
            } else {
                let mut end = cap;
                while end > 0 && !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                if let Some(prefix) = trimmed.get(..end) {
                    buf.push_str(prefix);
                }
                tracing::warn!(
                    target: LOG,
                    original = trimmed.len(),
                    cap,
                    "DREAM_BUILD_MESSAGE: existing memory truncated"
                );
            }
        }
    }

    let mut processed_stems = Vec::with_capacity(stems.len());
    for stem in stems {
        let path = sessions_dir.join(format!("{stem}.md"));
        if let Ok(content) = std::fs::read_to_string(&path)
            && !content.trim().is_empty()
        {
            if !buf.is_empty() {
                buf.push_str("\n\n");
            }
            buf.push_str("--- Session: ");
            buf.push_str(stem);
            buf.push_str(" ---\n\n");
            buf.push_str(&content);
            processed_stems.push(stem.clone());

            if buf.len() >= MAX_DREAM_INPUT_CHARS {
                tracing::warn!(
                    target: LOG,
                    sessions_read = processed_stems.len(),
                    total = stems.len(),
                    chars = buf.len(),
                    "DREAM_BUILD_MESSAGE: truncated input at {MAX_DREAM_INPUT_CHARS} chars"
                );
                break;
            }
        }
    }
    if processed_stems.is_empty() {
        tracing::debug!(target: LOG, "DREAM_BUILD_MESSAGE: no readable session files");
        return None;
    }
    tracing::info!(
        target: LOG,
        sessions_read = processed_stems.len(),
        total = stems.len(),
        chars = buf.len(),
        "DREAM_BUILD_MESSAGE: built user message"
    );
    Some(DreamMessage {
        content: buf,
        processed_stems,
    })
}

/// Output cap for dream responses; hardcoded for v1, move to `MemoryDreamConfig` if operational tuning is needed.
const MAX_DREAM_CHARS: usize = 16_000;

/// Process the dream model's response.
/// `write_long_term` writes content directly without normalization, so the dream's markdown structure is preserved as-is.
/// Truncates content exceeding [`MAX_DREAM_CHARS`].
pub fn process_dream_response(response: &str) -> Option<String> {
    let trimmed = response.trim();
    if trimmed.is_empty() {
        tracing::info!(target: LOG, "DREAM_RESPONSE: empty");
        return None;
    }

    if is_no_reply(trimmed) {
        tracing::info!(target: LOG, "DREAM_RESPONSE: NO_REPLY");
        return None;
    }

    if !has_markdown_headers(trimmed) {
        tracing::info!(
            target: LOG,
            len = trimmed.len(),
            "DREAM_RESPONSE: no markdown headers, rejected"
        );
        return None;
    }

    let char_count = trimmed.chars().count();
    let (content, accepted_chars) = if char_count > MAX_DREAM_CHARS {
        tracing::warn!(
            target: LOG,
            original = char_count,
            limit = MAX_DREAM_CHARS,
            "DREAM_RESPONSE: truncated"
        );
        (
            trimmed.chars().take(MAX_DREAM_CHARS).collect(),
            MAX_DREAM_CHARS,
        )
    } else {
        (trimmed.to_string(), char_count)
    };

    tracing::info!(
        target: LOG,
        chars = accepted_chars,
        "DREAM_RESPONSE: accepted"
    );
    Some(content)
}

/// Minimum age in seconds a session file must have before cleanup will delete it.
/// Protects files that a concurrent session may still be appending to.
const CLEANUP_RECENCY_GUARD_SECS: u64 = 300; // 5 minutes

/// Delete session log files whose stems were processed during dream.
/// Logs warnings for deletion failures but never propagates errors, because the consolidation already succeeded.
pub fn clean_processed_sessions(sessions_dir: &Path, stems: &[String]) -> Vec<String> {
    let mut cleaned = Vec::new();
    let now = SystemTime::now();
    for stem in stems {
        let path = sessions_dir.join(format!("{stem}.md"));

        // Skip files modified within the recency guard window; a concurrent session may still be appending
        if let Ok(meta) = std::fs::metadata(&path)
            && let Ok(mtime) = meta.modified()
            && now.duration_since(mtime).unwrap_or_default().as_secs() < CLEANUP_RECENCY_GUARD_SECS
        {
            tracing::debug!(
                target: LOG,
                path = %path.display(),
                "DREAM_CLEANUP: skipping recently-modified session file"
            );
            continue;
        }

        match std::fs::remove_file(&path) {
            Ok(()) => cleaned.push(stem.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone; not an error, but don't count it as removed
            }
            Err(e) => {
                tracing::warn!(
                    target: LOG,
                    path = %path.display(),
                    error = %e,
                    "DREAM_CLEANUP: failed to delete session file"
                );
            }
        }
    }
    if !cleaned.is_empty() {
        tracing::info!(
            target: LOG,
            cleaned = cleaned.len(),
            total = stems.len(),
            "DREAM_CLEANUP: removed processed session files"
        );
    }
    cleaned
}

/// Execute the dream around a provided model response.
///
/// The session actor makes the actual model call and passes the response here.
pub fn execute_dream(
    storage: &super::storage::MemoryStorage,
    response: &str,
    sessions_eligible: usize,
) -> DreamResult {
    let content = match process_dream_response(response) {
        Some(c) => c,
        None => {
            tracing::info!(target: LOG, sessions_eligible, "DREAM_EXECUTE: nothing to consolidate");
            return DreamResult {
                status: DreamStatus::NothingToConsolidate,
                sessions_eligible,
            };
        }
    };

    let chars_written = content.chars().count();
    if let Err(e) = storage.write_long_term(super::storage::MemoryScope::Workspace, &content) {
        tracing::warn!(target: LOG, error = %e, "DREAM_EXECUTE: write failed");
        return DreamResult {
            status: DreamStatus::Failed(format!("failed to write MEMORY.md: {e}")),
            sessions_eligible,
        };
    }

    tracing::info!(
        target: LOG,
        chars_written,
        sessions_eligible,
        "DREAM_EXECUTE: completed"
    );
    DreamResult {
        status: DreamStatus::Completed { chars_written },
        sessions_eligible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    // Mirrors `dream_lock::tests::write_session`; it is kept local to avoid cross-module test coupling for a trivial 5-line helper
    fn write_session(dir: &Path, name: &str, age_secs: u64) {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}.md"));
        fs::write(&path, "test").unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_secs);
        filetime::set_file_mtime(&path, FileTime::from_system_time(t)).unwrap();
    }

    fn enabled_config() -> MemoryDreamConfig {
        MemoryDreamConfig {
            enabled: true,
            min_hours: 24,
            min_sessions: 5,
            stale_lock_secs: 3600,
            check_interval_secs: None,
        }
    }

    fn set_consolidation_age(dir: &Path, age_secs: u64) {
        let marker = dir.join(".dream-consolidated");
        fs::write(&marker, "").unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_secs);
        filetime::set_file_mtime(&marker, FileTime::from_system_time(t)).unwrap();
    }

    #[test]
    fn disabled_config_returns_disabled() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("sessions");

        let config = MemoryDreamConfig {
            enabled: false,
            ..enabled_config()
        };
        assert_eq!(
            check_dream_gates(&config, &lock, &sessions, None),
            DreamGate::Disabled
        );
    }

    #[test]
    fn too_soon_when_recently_consolidated() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        lock.record_consolidation().unwrap();

        let sessions = dir.path().join("sessions");
        let config = enabled_config();
        let result = check_dream_gates(&config, &lock, &sessions, None);
        match result {
            DreamGate::TooSoon { hours_since } => {
                assert!(hours_since < config.min_hours);
            }
            other => panic!("expected TooSoon, got {other:?}"),
        }
    }

    #[test]
    fn too_few_sessions_when_under_threshold() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("sessions");

        set_consolidation_age(dir.path(), 48 * 3600);

        for i in 0..3 {
            write_session(&sessions, &format!("2026-01-0{i}-proj-aaa{i:05}"), 100);
        }

        let config = enabled_config();
        let result = check_dream_gates(&config, &lock, &sessions, None);
        assert_eq!(
            result,
            DreamGate::TooFewSessions {
                count: 3,
                required: 5,
            }
        );
    }

    #[test]
    fn all_gates_pass_returns_open_with_sessions() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("sessions");

        set_consolidation_age(dir.path(), 48 * 3600);

        for i in 0..6 {
            write_session(&sessions, &format!("2026-01-{i:02}-proj-aaa{i:05}"), 100);
        }

        let config = enabled_config();
        let result = check_dream_gates(&config, &lock, &sessions, None);
        match result {
            DreamGate::Open { sessions } => {
                assert_eq!(sessions.len(), 6);
                assert!(sessions.windows(2).all(|w| matches!(w, [a, b] if a <= b)));
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn no_prior_consolidation_treats_as_epoch() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("sessions");

        for i in 0..5 {
            write_session(&sessions, &format!("2026-01-0{i}-proj-aaa{i:05}"), 100);
        }

        let config = enabled_config();
        let result = check_dream_gates(&config, &lock, &sessions, None);
        match result {
            DreamGate::Open { sessions } => assert_eq!(sessions.len(), 5),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn nonexistent_sessions_dir_returns_too_few() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("nonexistent");

        let config = enabled_config();
        let result = check_dream_gates(&config, &lock, &sessions, None);
        assert_eq!(
            result,
            DreamGate::TooFewSessions {
                count: 0,
                required: 5,
            }
        );
    }

    #[test]
    fn current_session_excluded_from_count() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("sessions");

        set_consolidation_age(dir.path(), 48 * 3600);

        for i in 0..4 {
            write_session(&sessions, &format!("2026-01-0{i}-proj-aaa{i:05}"), 100);
        }
        write_session(&sessions, "2026-01-05-proj-current1", 100);

        let config = enabled_config();
        let result = check_dream_gates(&config, &lock, &sessions, Some("current1"));
        assert_eq!(
            result,
            DreamGate::TooFewSessions {
                count: 4,
                required: 5,
            }
        );
    }

    #[test]
    fn min_hours_boundary_exact_is_too_soon() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("sessions");

        let config = MemoryDreamConfig {
            min_hours: 24,
            ..enabled_config()
        };
        set_consolidation_age(dir.path(), 23 * 3600 + 59 * 60);

        let result = check_dream_gates(&config, &lock, &sessions, None);
        match result {
            DreamGate::TooSoon { hours_since } => assert_eq!(hours_since, 23),
            other => panic!("expected TooSoon, got {other:?}"),
        }
    }

    #[test]
    fn min_sessions_boundary_exact_passes() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("sessions");

        let config = MemoryDreamConfig {
            min_sessions: 3,
            ..enabled_config()
        };
        for i in 0..3 {
            write_session(&sessions, &format!("2026-01-0{i}-proj-aaa{i:05}"), 100);
        }

        let result = check_dream_gates(&config, &lock, &sessions, None);
        match result {
            DreamGate::Open { sessions } => assert_eq!(sessions.len(), 3),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn min_sessions_boundary_one_below_fails() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let sessions = dir.path().join("sessions");

        let config = MemoryDreamConfig {
            min_sessions: 3,
            ..enabled_config()
        };
        for i in 0..2 {
            write_session(&sessions, &format!("2026-01-0{i}-proj-aaa{i:05}"), 100);
        }

        let result = check_dream_gates(&config, &lock, &sessions, None);
        assert_eq!(
            result,
            DreamGate::TooFewSessions {
                count: 2,
                required: 3,
            }
        );
    }

    // -------------------------------------------------------------------
    // build_dream_user_message tests
    // -------------------------------------------------------------------

    fn write_session_content(dir: &Path, name: &str, content: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(format!("{name}.md")), content).unwrap();
    }

    /// Write a session file and back-date its mtime so it passes the recency guard in `clean_processed_sessions`.
    fn write_old_session_content(dir: &Path, name: &str, content: &str) {
        write_session_content(dir, name, content);
        let old = SystemTime::now() - Duration::from_secs(CLEANUP_RECENCY_GUARD_SECS + 60);
        let path = dir.join(format!("{name}.md"));
        filetime::set_file_mtime(&path, FileTime::from_system_time(old)).unwrap();
    }

    #[test]
    fn build_message_with_valid_sessions() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        write_session_content(&sessions, "sess-a", "Decision: use Rust");
        write_session_content(&sessions, "sess-b", "Architecture: event-driven");

        let stems = vec!["sess-a".to_string(), "sess-b".to_string()];
        let msg = build_dream_user_message(&sessions, &stems, None).unwrap();

        assert!(msg.content.contains("--- Session: sess-a ---"));
        assert!(msg.content.contains("Decision: use Rust"));
        assert!(msg.content.contains("--- Session: sess-b ---"));
        assert!(msg.content.contains("Architecture: event-driven"));
        assert_eq!(msg.processed_stems, vec!["sess-a", "sess-b"]);
    }

    #[test]
    fn build_message_skips_empty_sessions() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        write_session_content(&sessions, "real", "Some content");
        write_session_content(&sessions, "empty", "   ");

        let stems = vec!["real".to_string(), "empty".to_string()];
        let msg = build_dream_user_message(&sessions, &stems, None).unwrap();

        assert!(msg.content.contains("--- Session: real ---"));
        assert!(!msg.content.contains("--- Session: empty ---"));
        assert_eq!(msg.processed_stems, vec!["real"]);
    }

    #[test]
    fn build_message_returns_none_for_no_readable_files() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let stems = vec!["missing-a".to_string(), "missing-b".to_string()];
        assert!(build_dream_user_message(&sessions, &stems, None).is_none());
    }

    #[test]
    fn build_message_mixed_readable_and_missing() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        write_session_content(&sessions, "exists", "Important info");

        let stems = vec!["exists".to_string(), "ghost".to_string()];
        let msg = build_dream_user_message(&sessions, &stems, None).unwrap();

        assert!(msg.content.contains("Important info"));
        assert!(!msg.content.contains("ghost"));
        assert_eq!(msg.processed_stems, vec!["exists"]);
    }

    // -------------------------------------------------------------------
    // process_dream_response tests
    // -------------------------------------------------------------------

    #[test]
    fn process_empty_response_returns_none() {
        assert!(process_dream_response("").is_none());
        assert!(process_dream_response("   ").is_none());
        assert!(process_dream_response("\n\n").is_none());
    }

    #[test]
    fn process_no_reply_returns_none() {
        assert!(process_dream_response("NO_REPLY").is_none());
        assert!(process_dream_response("no reply").is_none());
        assert!(process_dream_response("No-Reply").is_none());
        assert!(process_dream_response("  NO_REPLY  ").is_none());
    }

    #[test]
    fn process_valid_markdown_returns_content() {
        let input = "## Topic A\n\nSome insight.\n\n## Topic B\n\nAnother insight.";
        let result = process_dream_response(input).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn process_h1_header_accepted() {
        let input = "# Top Level\n\nSome content.";
        assert_eq!(process_dream_response(input).unwrap(), input);
    }

    #[test]
    fn process_no_headers_returns_none() {
        assert!(process_dream_response("Just plain text without headers.").is_none());
    }

    #[test]
    fn process_truncates_long_content() {
        let header = "## Long\n\n";
        let body = "x".repeat(MAX_DREAM_CHARS + 1000);
        let input = format!("{header}{body}");

        let result = process_dream_response(&input).unwrap();
        assert_eq!(result.chars().count(), MAX_DREAM_CHARS);
        assert!(result.starts_with("## Long"));
    }

    #[test]
    fn process_exactly_at_limit_not_truncated() {
        let header = "## H\n\n";
        let remaining = MAX_DREAM_CHARS - header.len();
        let body = "a".repeat(remaining);
        let input = format!("{header}{body}");

        let result = process_dream_response(&input).unwrap();
        assert_eq!(result.chars().count(), MAX_DREAM_CHARS);
        assert_eq!(result, input);
    }

    // -------------------------------------------------------------------
    // execute_dream tests
    // -------------------------------------------------------------------

    use super::super::storage::MemoryStorage;
    use std::path::PathBuf;

    fn test_storage(tmp: &TempDir) -> (MemoryStorage, PathBuf) {
        let global = tmp.path().join("memory");
        let workspace = global.join("test-ws");
        let storage = MemoryStorage::with_paths(global, workspace.clone());
        (storage, workspace)
    }

    #[test]
    fn execute_dream_valid_response_writes_memory() {
        let dir = TempDir::new().unwrap();
        let (storage, ws) = test_storage(&dir);

        let response = "## Decisions\n\nWe chose Rust.\n\n## Architecture\n\nEvent-driven.";
        let result = execute_dream(&storage, response, 5);

        assert!(
            matches!(result.status, DreamStatus::Completed { chars_written } if chars_written == response.chars().count())
        );
        assert_eq!(result.sessions_eligible, 5);

        let memory = fs::read_to_string(ws.join("MEMORY.md")).unwrap();
        assert!(memory.contains("We chose Rust."));
        assert!(memory.contains("Event-driven."));
    }

    #[test]
    fn execute_dream_empty_response_nothing_to_consolidate() {
        let dir = TempDir::new().unwrap();
        let (storage, _) = test_storage(&dir);

        let result = execute_dream(&storage, "", 3);
        assert_eq!(result.status, DreamStatus::NothingToConsolidate);
        assert_eq!(result.sessions_eligible, 3);
    }

    #[test]
    fn execute_dream_write_failure_returns_failed() {
        let dir = TempDir::new().unwrap();

        // Point the workspace dir at a path under a regular file so create_dir_all inside append_to_memory fails
        // This works even when running as root, unlike chmod-based approaches
        let blocker = dir.path().join("memory").join("blocker");
        fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        fs::write(&blocker, "I am a file").unwrap();
        let workspace = blocker.join("impossible-subdir");

        let storage = MemoryStorage::with_paths(dir.path().join("memory"), workspace);

        let result = execute_dream(&storage, "## Topic\n\nContent", 3);

        match &result.status {
            DreamStatus::Failed(reason) => {
                assert!(reason.contains("MEMORY.md"), "reason: {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn execute_dream_overwrites_existing_memory() {
        let dir = TempDir::new().unwrap();
        let (storage, ws) = test_storage(&dir);

        // Pre-populate MEMORY.md
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("MEMORY.md"), "## Existing\n\nOld content.").unwrap();

        let response = "## New Topic\n\nFresh insight.";
        let result = execute_dream(&storage, response, 2);

        assert!(matches!(result.status, DreamStatus::Completed { .. }));

        let memory = fs::read_to_string(ws.join("MEMORY.md")).unwrap();
        // write_long_term overwrites rather than appends
        assert!(!memory.contains("Old content."));
        assert_eq!(memory.trim(), response);
    }

    #[test]
    fn build_message_respects_input_size_cap() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        let big_content = "x".repeat(MAX_DREAM_INPUT_CHARS);
        write_session_content(&sessions, "big", &big_content);
        write_session_content(&sessions, "small", "should not appear");

        let stems = vec!["big".to_string(), "small".to_string()];
        let msg = build_dream_user_message(&sessions, &stems, None).unwrap();

        assert!(msg.content.len() >= MAX_DREAM_INPUT_CHARS);
        assert!(
            !msg.content.contains("should not appear"),
            "second session should be skipped after cap"
        );
        // The second stem was beyond the 32K cap and must not be cleaned up
        assert_eq!(msg.processed_stems, vec!["big"]);
    }

    // -------------------------------------------------------------------
    // Session cleanup tests
    // -------------------------------------------------------------------

    #[test]
    fn cleanup_deletes_processed_sessions() {
        let dir = TempDir::new().unwrap();

        // Create session files that will be "processed" (old mtime to pass the recency guard)
        let sessions = dir.path().join("sessions");
        write_old_session_content(&sessions, "sess-a", "Content A");
        write_old_session_content(&sessions, "sess-b", "Content B");

        let processed = vec!["sess-a".to_string(), "sess-b".to_string()];
        let cleaned = clean_processed_sessions(&sessions, &processed);
        assert_eq!(cleaned.len(), 2);

        // Both files are gone
        assert!(!sessions.join("sess-a.md").exists());
        assert!(!sessions.join("sess-b.md").exists());
    }

    #[test]
    fn cleanup_preserves_unprocessed_sessions() {
        let dir = TempDir::new().unwrap();

        // Create session files; only some will be "processed"
        let sessions = dir.path().join("sessions");
        write_old_session_content(&sessions, "processed", "Content");
        write_old_session_content(&sessions, "unprocessed", "Kept content");

        // Only mark "processed" as having been read
        let processed = vec!["processed".to_string()];
        let cleaned = clean_processed_sessions(&sessions, &processed);
        assert_eq!(cleaned.len(), 1);

        // The processed file is gone; the unprocessed one remains
        assert!(!sessions.join("processed.md").exists());
        assert!(sessions.join("unprocessed.md").exists());
        assert_eq!(
            fs::read_to_string(sessions.join("unprocessed.md")).unwrap(),
            "Kept content"
        );
    }

    #[test]
    fn cleanup_failure_returns_only_deleted() {
        let dir = TempDir::new().unwrap();

        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // Create a directory where a .md file is expected; remove_file on a directory fails even as root, so cleanup fails without relying on chmod
        // Back-date the dir's mtime so it passes the recency guard
        let bad_path = sessions.join("bad-stem.md");
        fs::create_dir_all(&bad_path).unwrap();
        let old = SystemTime::now() - Duration::from_secs(CLEANUP_RECENCY_GUARD_SECS + 60);
        filetime::set_file_mtime(&bad_path, FileTime::from_system_time(old)).unwrap();

        // Also create a normal file that can be cleaned
        write_old_session_content(&sessions, "good-stem", "Content");

        let processed = vec!["bad-stem".to_string(), "good-stem".to_string()];
        let cleaned = clean_processed_sessions(&sessions, &processed);

        // Only the good-stem file was cleaned; bad-stem failed and is excluded
        assert_eq!(cleaned, vec!["good-stem"]);
        // The directory-as-file still exists because its cleanup failed
        assert!(sessions.join("bad-stem.md").exists());
        assert!(!sessions.join("good-stem.md").exists());
    }

    #[test]
    fn cleanup_skips_recently_modified_files() {
        let dir = TempDir::new().unwrap();

        let sessions = dir.path().join("sessions");
        // This file has the current mtime, within the recency guard window
        write_session_content(&sessions, "recent", "Still being written");
        // This file is old enough to be cleaned
        write_old_session_content(&sessions, "old", "Done");

        let processed = vec!["recent".to_string(), "old".to_string()];
        let cleaned = clean_processed_sessions(&sessions, &processed);

        assert_eq!(cleaned, vec!["old"]);
        assert!(
            sessions.join("recent.md").exists(),
            "recently-modified file must be preserved"
        );
        assert!(!sessions.join("old.md").exists());
    }

    #[test]
    fn build_message_processed_stems_excludes_capped_sessions() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");

        // The first session fills the buffer past the cap
        let big_content = "x".repeat(MAX_DREAM_INPUT_CHARS);
        write_session_content(&sessions, "first", &big_content);
        // The second session lands over the cap and must not be processed
        write_session_content(&sessions, "second", "small content");
        // The third session must not be processed either
        write_session_content(&sessions, "third", "more content");

        let stems = vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ];
        let msg = build_dream_user_message(&sessions, &stems, None).unwrap();

        assert_eq!(
            msg.processed_stems,
            vec!["first"],
            "only the first session should be in processed_stems"
        );
        assert_eq!(msg.processed_stems.len(), 1);
    }

    #[test]
    fn end_to_end_cap_boundary_cleanup() {
        // Integration test: build_dream_user_message hits the 32K cap partway through the stems list
        let dir = TempDir::new().unwrap();
        let (storage, _ws) = test_storage(&dir);

        let sessions = dir.path().join("sessions");

        // Create 5 session files: the first 2 fill past the cap, sessions 3-5 should survive cleanup
        let half_cap = MAX_DREAM_INPUT_CHARS / 2 + 500; // Two sessions of this size exceed the cap.
        write_old_session_content(&sessions, "aaa-first", &"a".repeat(half_cap));
        write_old_session_content(&sessions, "bbb-second", &"b".repeat(half_cap));
        write_old_session_content(&sessions, "ccc-third", "small content 3");
        write_old_session_content(&sessions, "ddd-fourth", "small content 4");
        write_old_session_content(&sessions, "eee-fifth", "small content 5");

        let all_stems: Vec<String> = vec![
            "aaa-first",
            "bbb-second",
            "ccc-third",
            "ddd-fourth",
            "eee-fifth",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        // Phase 1: build_dream_user_message should cap after the first 2 sessions
        let dream_msg = build_dream_user_message(&sessions, &all_stems, None).unwrap();
        assert_eq!(
            dream_msg.processed_stems.len(),
            2,
            "only first 2 sessions should fit within 32K cap"
        );
        assert_eq!(
            dream_msg.processed_stems.first().map(String::as_str),
            Some("aaa-first")
        );
        assert_eq!(
            dream_msg.processed_stems.get(1).map(String::as_str),
            Some("bbb-second")
        );

        let response = "## Consolidated\n\nMerged from 2 sessions.";
        let result = execute_dream(&storage, response, all_stems.len());
        assert!(matches!(result.status, DreamStatus::Completed { .. }));

        let cleaned = clean_processed_sessions(&sessions, &dream_msg.processed_stems);
        assert_eq!(cleaned.len(), 2);

        // The processed files are deleted
        assert!(!sessions.join("aaa-first.md").exists());
        assert!(!sessions.join("bbb-second.md").exists());

        // The unprocessed files are preserved
        assert!(sessions.join("ccc-third.md").exists());
        assert!(sessions.join("ddd-fourth.md").exists());
        assert!(sessions.join("eee-fifth.md").exists());
    }

    #[test]
    fn clean_processed_sessions_handles_already_missing_files() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        // Only create one of the two stems (old mtime to pass the recency guard)
        write_old_session_content(&sessions, "exists", "Content");

        // Pass both; "missing" doesn't exist and should not error or count
        let cleaned =
            clean_processed_sessions(&sessions, &["exists".to_string(), "missing".to_string()]);

        assert_eq!(
            cleaned.len(),
            1,
            "only the existing file should count as cleaned"
        );
        assert_eq!(cleaned.first().map(String::as_str), Some("exists"));
        assert!(!sessions.join("exists.md").exists());
    }

    // -------------------------------------------------------------------
    // is_scaffold_template tests
    // -------------------------------------------------------------------

    #[test]
    fn scaffold_detects_old_workspace_template() {
        assert!(is_scaffold_template(
            "# Project Memory\n\n## Project Context\n\n<!-- Add project-specific knowledge here -->\n",
        ));
    }

    #[test]
    fn scaffold_detects_new_workspace_template() {
        assert!(is_scaffold_template(
            "# Project Memory — /test\n\n> Auto-populated by dream consolidation. Edit freely.\n",
        ));
    }

    #[test]
    fn scaffold_detects_global_template() {
        assert!(is_scaffold_template(
            "# Global Memory\n\n## Preferences\n\n<!-- Add any cross-project preferences here -->\n",
        ));
    }

    #[test]
    fn scaffold_rejects_real_content() {
        assert!(!is_scaffold_template(
            "## Decisions\n\nWe chose Rust.\n\n## Architecture\n\nEvent-driven.",
        ));
    }

    #[test]
    fn scaffold_boundary_499_bytes_is_scaffold() {
        let marker = "Add project-specific knowledge here";
        let pad_len = 499 - marker.len();
        let content = format!("{}{}", "x".repeat(pad_len), marker);
        assert_eq!(content.trim().len(), 499);
        assert!(
            is_scaffold_template(&content),
            "499-byte content with marker must be classified as scaffold"
        );
    }

    #[test]
    fn scaffold_boundary_500_bytes_is_not_scaffold() {
        let marker = "Add project-specific knowledge here";
        let pad_len = 500 - marker.len();
        let content = format!("{}{}", "x".repeat(pad_len), marker);
        assert_eq!(content.trim().len(), 500);
        assert!(
            !is_scaffold_template(&content),
            "500-byte content with marker must NOT be classified as scaffold"
        );
    }

    #[test]
    fn scaffold_rejects_large_content_with_leftover_marker() {
        let real_content = "x".repeat(600);
        let content_with_marker = format!(
            "# Project Memory\n\n<!-- Add project-specific knowledge here -->\n\n## Decisions\n\n{}",
            real_content
        );
        assert!(
            !is_scaffold_template(&content_with_marker),
            "large file with leftover scaffold marker must NOT be classified as scaffold"
        );
    }

    #[test]
    fn scaffold_rejects_large_workspace_template_with_real_content() {
        let real_content = "a ".repeat(300);
        let content = format!(
            "# Project Memory — /test\n\n> Auto-populated by dream consolidation. Edit freely.\n\n## Architecture\n\n{}",
            real_content
        );
        assert!(
            !is_scaffold_template(&content),
            "workspace template with substantial appended content must not be scaffold"
        );
    }

    // -------------------------------------------------------------------
    // build_dream_user_message with existing memory tests
    // -------------------------------------------------------------------

    #[test]
    fn build_message_prepends_existing_memory() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        write_session_content(&sessions, "sess-a", "Decision: use Rust");

        let stems = vec!["sess-a".to_string()];
        let existing = "## Prior\n\nOld knowledge.";
        let msg = build_dream_user_message(&sessions, &stems, Some(existing)).unwrap();

        assert!(msg.content.starts_with("--- Existing Memory"));
        assert!(msg.content.contains("Old knowledge."));
        assert!(msg.content.contains("Decision: use Rust"));
        let mem_pos = msg.content.find("Old knowledge.").unwrap();
        let sess_pos = msg.content.find("Decision: use Rust").unwrap();
        assert!(mem_pos < sess_pos, "existing memory must precede sessions");
    }

    #[test]
    fn build_message_skips_scaffold_existing_memory() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        write_session_content(&sessions, "sess-a", "Decision: use Rust");

        let stems = vec!["sess-a".to_string()];
        let scaffold = "# Project Memory\n\n<!-- Add project-specific knowledge here -->\n";
        let msg = build_dream_user_message(&sessions, &stems, Some(scaffold)).unwrap();

        assert!(!msg.content.contains("Existing Memory"));
        assert!(msg.content.contains("Decision: use Rust"));
    }

    #[test]
    fn build_message_includes_large_memory_with_scaffold_marker() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        write_session_content(&sessions, "sess-a", "Decision: use Rust");

        let stems = vec!["sess-a".to_string()];
        let real_content = "x".repeat(600);
        let memory = format!(
            "# Project Memory\n\n<!-- Add project-specific knowledge here -->\n\n## Architecture\n\n{}",
            real_content
        );
        let msg = build_dream_user_message(&sessions, &stems, Some(&memory)).unwrap();

        assert!(
            msg.content.contains("Existing Memory"),
            "large memory with leftover scaffold marker must be included"
        );
        assert!(msg.content.contains(&real_content));
    }

    #[test]
    fn build_message_returns_none_with_existing_memory_but_no_sessions() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let stems = vec!["missing".to_string()];
        let existing = "## Prior\n\nOld knowledge.";
        assert!(
            build_dream_user_message(&sessions, &stems, Some(existing)).is_none(),
            "should return None when no sessions are readable, even with existing memory"
        );
    }
}
