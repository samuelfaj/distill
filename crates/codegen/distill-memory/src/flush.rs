// Modified for Distill by Samuel Fajreldines, 2026.
//! Pure pre-compaction memory flush policy and response processing.

use crate::{
    MemoryIndex,
    embedding::EmbeddingProvider,
    text_utils::{has_markdown_headers, is_no_reply},
};
use distill_config_types::MemoryFlushConfig;

const LOG: &str = "distill_memory";

/// Check whether a memory flush should run before the next compaction.
/// A flush runs at most once per compaction cycle, once token usage reaches the compact threshold minus `soft_threshold_tokens` headroom.
/// The flush threshold sits below the compact threshold so the flush completes before the context window overflows.
pub fn should_flush(
    total_tokens: u64,
    context_window: u64,
    compact_threshold_percent: u8,
    flush_config: &MemoryFlushConfig,
    last_flush_compaction: u64,
    current_compaction_count: u64,
) -> bool {
    if !flush_config.enabled {
        tracing::debug!(target: LOG, "MEMORY_FLUSH_CHECK: disabled");
        return false;
    }
    if last_flush_compaction == current_compaction_count {
        tracing::debug!(target: LOG,
            "MEMORY_FLUSH_CHECK: already flushed this cycle (cycle={current_compaction_count})");
        return false;
    }
    let should = distill_token_estimation::exceeds_threshold_with_headroom(
        total_tokens,
        context_window,
        compact_threshold_percent,
        flush_config.soft_threshold_tokens,
    );
    // This threshold is only for the log; the decision above uses scaled arithmetic and may differ by 1 token at non-round windows
    let flush_threshold = context_window
        .saturating_mul(compact_threshold_percent as u64)
        .saturating_sub(flush_config.soft_threshold_tokens.saturating_mul(100))
        / 100;
    tracing::info!(target: LOG,
        "MEMORY_FLUSH_CHECK: tokens={total_tokens} threshold={flush_threshold} \
         window={context_window} pct={compact_threshold_percent} soft={soft} -> {result}",
        soft = flush_config.soft_threshold_tokens,
        result = if should { "FLUSH" } else { "skip" },
    );
    should
}

// ---------------------------------------------------------------------------
// Flush prompt and response processing
// ---------------------------------------------------------------------------

/// System prompt injected for the flush model call.
pub const FLUSH_SYSTEM_PROMPT: &str = "\
You are a memory assistant. Extract ALL useful information from this conversation \
that would help you be more effective in future sessions with this user. \
Write a concise markdown summary with ## headers covering:

- **Decisions & rationale** — what was chosen and why
- **Technical context** — architecture, APIs, patterns, tools, file paths discussed
- **Debugging techniques & tools** — external APIs, CLI commands, query patterns, \
investigation workflows, or services discovered or used during debugging
- **Problems & solutions** — bugs found, how they were fixed, workarounds

Prioritize reusable mechanisms, rules, and root causes over a narration of the task. \
When project structure matters, name the concrete directories and stable repo-relative paths; \
include an absolute workspace root only when it is operationally necessary and appears in the conversation. \
Copy exact identifiers and numerical values only when they appear verbatim in the supplied conversation. \
Never infer a missing value; omit the detail instead.

Omit any section where there is nothing substantive to report. \
Do NOT include user preferences like OS, shell, or editor — these belong in global memory. \
Do NOT include an ephemeral progress section — transient status is not useful for future sessions.

Respond with NO_REPLY if nothing genuinely useful was learned — a routine task \
that followed standard patterns, brief Q&A, or sessions with no novel decisions \
or discoveries are not worth persisting. Only write content that a future session \
would concretely benefit from.";

/// System prompt for incremental (delta) flushes after the first flush.
/// It applies when `flush_count > 0` and previous flush content is available.
/// The caller appends the previous flush output after this prompt.
pub const FLUSH_DELTA_SYSTEM_PROMPT: &str = "\
You are a memory assistant performing an incremental update. The previous \
flush output for this session is shown below. Extract ONLY information that \
is NEW since the previous flush — do not repeat anything already captured.

Write a concise markdown summary with ## headers covering only NEW items in:
- **Decisions & rationale** — new decisions since last flush
- **Technical context** — new architecture, APIs, patterns discovered
- **Debugging techniques** — new techniques used since last flush
- **Problems & solutions** — new bugs found and fixes

Prioritize reusable mechanisms, rules, and root causes over a narration of the task. \
When project structure matters, name the concrete directories and stable repo-relative paths; \
include an absolute workspace root only when it is operationally necessary and appears in the conversation. \
Copy exact identifiers and numerical values only when they appear verbatim in the supplied conversation; \
never reconstruct or guess missing values.

Omit any section that has no new content. Do NOT include user preferences \
like OS, shell, or editor — these are captured in global memory.
Do NOT include 'Current state' — this is ephemeral and not useful for future sessions.

Respond with NO_REPLY if nothing genuinely new and useful has happened since \
the previous flush. Routine changes that follow standard patterns are not worth \
an incremental update.

--- Previous flush content ---
";

/// Result of processing the model's flush response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushResult {
    /// Model indicated nothing to store (empty response or NO_REPLY).
    NothingToStore,
    /// Response was accepted after quality checks. Contains the content to write.
    /// The caller should run [`is_semantically_duplicate()`] before writing.
    Accepted(String),
    /// Response was rejected by quality controls.
    Rejected(String),
}

/// Process the model's flush response, applying quality controls.
/// A response without a markdown header (`##`) is `Rejected`.
pub fn process_flush_response(response: &str, config: &MemoryFlushConfig) -> FlushResult {
    let trimmed = response.trim();
    let len = trimmed.len();
    let preview: String = trimmed.chars().take(200).collect();

    tracing::info!(target: LOG,
        "MEMORY_FLUSH_RESPONSE: len={len} preview=\"{preview}\"");

    if trimmed.is_empty() {
        tracing::info!(target: LOG,
            "MEMORY_FLUSH_RESPONSE: empty → NothingToStore");
        return FlushResult::NothingToStore;
    }

    if is_no_reply(trimmed) {
        tracing::info!(target: LOG,
            "MEMORY_FLUSH_RESPONSE: matches NO_REPLY pattern → NothingToStore");
        return FlushResult::NothingToStore;
    }

    // The length check counts chars, not bytes, to match the .chars().take() truncation
    let content = if trimmed.chars().count() > config.max_flush_write_chars {
        tracing::warn!(target: LOG,
            "MEMORY_FLUSH_RESPONSE: truncated from {len} to {} chars",
            config.max_flush_write_chars);
        trimmed
            .chars()
            .take(config.max_flush_write_chars)
            .collect::<String>()
    } else {
        trimmed.to_string()
    };

    if !has_markdown_headers(&content) {
        tracing::info!(target: LOG,
            "MEMORY_FLUSH_RESPONSE: no markdown headers → Rejected");
        return FlushResult::Rejected(
            "flush response lacks markdown structure (no ## headers)".to_string(),
        );
    }

    tracing::info!(target: LOG,
        "MEMORY_FLUSH_RESPONSE: accepted ({} chars, has headers)", content.len());
    FlushResult::Accepted(content)
}

/// Cosine similarity above which flush content counts as a semantic duplicate of an existing memory chunk.
/// 0.92 is conservative: it catches near-identical rephrasings while letting content with meaningful new information through.
/// This is the fallback when no config override is set.
pub const SEMANTIC_DEDUP_SIMILARITY_THRESHOLD: f64 = 0.92;

/// Maximum L2 distance between two unit-norm embedding vectors (used to convert sqlite-vec L2 distances to cosine similarity).
const MAX_L2_DISTANCE: f64 = 2.0;

/// Number of nearest neighbors to check during semantic dedup.
const SEMANTIC_DEDUP_KNN_LIMIT: usize = 3;

/// Check if flush content is semantically similar to existing memory chunks.
/// `threshold` is the cosine similarity cutoff (0.0 to 1.0); a KNN neighbor above it makes the content a duplicate.
/// The sync/async/sync phasing means `&MemoryIndex` is never held across an `.await`; it contains a `!Send` `rusqlite::Connection`.
pub async fn is_semantically_duplicate(
    content: &str,
    index: &MemoryIndex,
    embedding_provider: Option<&dyn EmbeddingProvider>,
    threshold: f64,
) -> bool {
    // Phase 1 (sync): check prerequisites; borrows index, no .await
    let provider = match embedding_provider {
        Some(p) => p,
        None => {
            tracing::debug!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: no embedding provider, skipping");
            return false;
        }
    };

    if !index.vec_available() {
        tracing::debug!(target: LOG,
            "MEMORY_FLUSH_SEMANTIC_DEDUP: sqlite-vec not available, skipping");
        return false;
    }

    // Phase 2 (async): embed; no &index borrow across this .await
    let embedding = match provider.embed_batch(&[content]).await {
        Ok(mut vecs) if !vecs.is_empty() => vecs.swap_remove(0),
        Ok(_) => {
            tracing::warn!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: embedding returned empty result");
            return false;
        }
        Err(e) => {
            tracing::warn!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: embedding failed: {e}");
            return false;
        }
    };

    // Phase 3 (sync): vector search + threshold check; borrows index, no .await
    let neighbors = match index.vector_search(&embedding, SEMANTIC_DEDUP_KNN_LIMIT) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: vector search failed: {e}");
            return false;
        }
    };

    let mut max_sim = 0.0_f64;
    for (chunk_id, distance) in &neighbors {
        let similarity = (1.0 - (*distance as f64 / MAX_L2_DISTANCE)).clamp(0.0, 1.0);
        max_sim = max_sim.max(similarity);
        if similarity > threshold {
            tracing::info!(target: LOG,
                "MEMORY_FLUSH_SEMANTIC_DEDUP: duplicate detected \
                 (chunk={chunk_id}, similarity={similarity:.4}, \
                 threshold={threshold})");
            return true;
        }
    }

    tracing::info!(target: LOG,
        "MEMORY_FLUSH_SEMANTIC_DEDUP: no duplicate \
         (checked={}, max_similarity={max_sim:.4}, \
         threshold={threshold})",
        neighbors.len());
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_flush_config() -> MemoryFlushConfig {
        MemoryFlushConfig::default()
    }

    #[test]
    fn test_should_flush_disabled() {
        let config = MemoryFlushConfig {
            enabled: false,
            ..default_flush_config()
        };
        assert!(!should_flush(90_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_already_flushed_this_cycle() {
        let config = default_flush_config();
        // Equal counters mean the flush already ran this cycle
        assert!(!should_flush(90_000, 100_000, 85, &config, 1, 1));
    }

    #[test]
    fn test_should_flush_below_threshold() {
        let config = default_flush_config();
        // 85% of the 100K window is 85K; minus the default 4K soft threshold, the flush point is 81K
        assert!(!should_flush(50_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_at_threshold() {
        let config = default_flush_config();
        // 85% of the 100K window is 85K; minus the default 4K soft threshold, the flush point is 81K
        assert!(should_flush(81_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_above_threshold() {
        let config = default_flush_config();
        assert!(should_flush(83_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_custom_soft_threshold() {
        let config = MemoryFlushConfig {
            soft_threshold_tokens: 10_000,
            ..default_flush_config()
        };
        // 85% of the 100K window is 85K; minus the 10K soft threshold, the flush point is 75K
        assert!(!should_flush(74_000, 100_000, 85, &config, 0, 1));
        assert!(should_flush(75_000, 100_000, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_different_compaction_cycles() {
        let config = default_flush_config();
        // The counter is pre-incremented to 1 in run_compact, so the first cycle sees (0, 1)
        assert!(should_flush(82_000, 100_000, 85, &config, 0, 1));
        // After the flush the counters match, blocking a second flush this cycle
        assert!(!should_flush(82_000, 100_000, 85, &config, 1, 1));
        // New cycle: should flush again
        assert!(should_flush(82_000, 100_000, 85, &config, 1, 2));
    }

    #[test]
    fn test_should_flush_non_round_window() {
        // With cw=10_001, pct=85, soft=4_000, the scaled boundary is used*100 >= 10_001*85 - 4_000*100 = 450_085, so the flush starts at used 4_501
        let config = MemoryFlushConfig {
            soft_threshold_tokens: 4_000,
            ..default_flush_config()
        };
        assert!(!should_flush(4_499, 10_001, 85, &config, 0, 1));
        assert!(!should_flush(4_500, 10_001, 85, &config, 0, 1));
        assert!(should_flush(4_501, 10_001, 85, &config, 0, 1));
    }

    #[test]
    fn test_should_flush_same_counter_values_blocks() {
        let config = default_flush_config();
        // Equal counters fire the "already flushed this cycle" guard
        // Both at 0 is the initial state; the pre-increment in maybe_pre_compaction_flush() keeps this from blocking the first flush
        assert!(!should_flush(82_000, 100_000, 85, &config, 0, 0));
        assert!(!should_flush(82_000, 100_000, 85, &config, 5, 5));
    }

    // -----------------------------------------------------------------------
    // process_flush_response tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_flush_response_empty() {
        let config = default_flush_config();
        assert_eq!(
            process_flush_response("", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("   ", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("\n\n", &config),
            FlushResult::NothingToStore
        );
    }

    #[test]
    fn test_flush_response_no_reply_variants() {
        let config = default_flush_config();
        assert_eq!(
            process_flush_response("NO_REPLY", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("no reply", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("No-Reply", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("noreply", &config),
            FlushResult::NothingToStore
        );
        assert_eq!(
            process_flush_response("  NO_REPLY  ", &config),
            FlushResult::NothingToStore
        );
    }

    #[test]
    fn test_flush_response_accepted() {
        let config = default_flush_config();
        let content = "## Key Decisions\n\nWe chose Rust for performance.";
        assert_eq!(
            process_flush_response(content, &config),
            FlushResult::Accepted(content.to_string())
        );
    }

    #[test]
    fn test_flush_response_rejected_no_headers() {
        let config = default_flush_config();
        let content = "Just some plain text without any markdown headers at all.";
        assert!(matches!(
            process_flush_response(content, &config),
            FlushResult::Rejected(_)
        ));
    }

    #[test]
    fn test_flush_response_truncated() {
        let config = MemoryFlushConfig {
            max_flush_write_chars: 50,
            ..default_flush_config()
        };
        let content = format!("# Title\n\n{}", "x".repeat(100));
        let result = process_flush_response(&content, &config);
        if let FlushResult::Accepted(text) = result {
            assert!(text.chars().count() <= 50);
        } else {
            panic!("expected Accepted, got {result:?}");
        }
    }

    #[test]
    fn test_flush_response_h1_header_accepted() {
        let config = default_flush_config();
        let content = "# Top Level\n\nSome content.";
        assert!(matches!(
            process_flush_response(content, &config),
            FlushResult::Accepted(_)
        ));
    }

    // -----------------------------------------------------------------------
    // is_semantically_duplicate tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_semantic_dedup_no_provider_allows_write() {
        use crate::{MemoryIndex, MemoryStorage, index::init_sqlite_vec};
        use tempfile::TempDir;

        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage =
            MemoryStorage::with_paths(tmp.path().join("global"), tmp.path().join("workspace"));
        let index = MemoryIndex::open_or_create(&db_path, storage, Default::default(), 4).unwrap();

        // With no embedding provider the check always returns false (allow write)
        let result = is_semantically_duplicate(
            "## Test\n\nSome content.",
            &index,
            None,
            SEMANTIC_DEDUP_SIMILARITY_THRESHOLD,
        )
        .await;
        assert!(!result, "should allow write when no embedding provider");
    }

    #[tokio::test]
    async fn test_semantic_dedup_no_similar_content() {
        use crate::embedding::MockEmbeddingProvider;
        use crate::{MemoryIndex, MemoryStorage, index::init_sqlite_vec};
        use tempfile::TempDir;

        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage =
            MemoryStorage::with_paths(tmp.path().join("global"), tmp.path().join("workspace"));
        let index = MemoryIndex::open_or_create(&db_path, storage, Default::default(), 4).unwrap();

        let provider = MockEmbeddingProvider { dimensions: 4 };

        // The empty index returns no neighbors, so nothing is a duplicate
        let result = is_semantically_duplicate(
            "## New Content\n\nFresh ideas here.",
            &index,
            Some(&provider),
            SEMANTIC_DEDUP_SIMILARITY_THRESHOLD,
        )
        .await;
        assert!(!result, "should not be duplicate against empty index");
    }

    #[tokio::test]
    async fn test_semantic_dedup_detects_identical_content() {
        use crate::embedding::{EmbeddingProvider, MockEmbeddingProvider};
        use crate::{MemoryIndex, MemoryStorage, index::init_sqlite_vec};
        use tempfile::TempDir;

        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage =
            MemoryStorage::with_paths(tmp.path().join("global"), tmp.path().join("workspace"));
        let mut index =
            MemoryIndex::open_or_create(&db_path, storage, Default::default(), 4).unwrap();

        let provider = MockEmbeddingProvider { dimensions: 4 };
        let content = "## Decisions\n\nWe chose Rust for memory safety.";

        // Index a file containing the same content.
        let file_path = tmp.path().join("existing.md");
        std::fs::write(&file_path, content).unwrap();
        index.reindex_file(&file_path, "session").unwrap();

        // Embed the existing chunk.
        let existing_embedding = provider.embed_batch(&[content]).await.unwrap();
        let chunk_id = format!("{}:0", file_path.to_string_lossy());
        let Some(emb0) = existing_embedding.first() else {
            panic!("embed_batch returns one vector per input: {existing_embedding:?}");
        };
        index.upsert_embedding(&chunk_id, emb0).unwrap();

        // Identical content embeds to the same vector, so the similarity is 1.0
        let result = is_semantically_duplicate(
            content,
            &index,
            Some(&provider),
            SEMANTIC_DEDUP_SIMILARITY_THRESHOLD,
        )
        .await;
        assert!(result, "identical content should be detected as duplicate");
    }

    #[tokio::test]
    async fn test_semantic_dedup_allows_different_content() {
        use crate::embedding::{EmbeddingProvider, MockEmbeddingProvider};
        use crate::{MemoryIndex, MemoryStorage, index::init_sqlite_vec};
        use tempfile::TempDir;

        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage =
            MemoryStorage::with_paths(tmp.path().join("global"), tmp.path().join("workspace"));
        let mut index =
            MemoryIndex::open_or_create(&db_path, storage, Default::default(), 4).unwrap();

        let provider = MockEmbeddingProvider { dimensions: 4 };
        let existing = "## Decisions\n\nWe chose Rust for memory safety.";

        // Index and embed existing content.
        let file_path = tmp.path().join("existing.md");
        std::fs::write(&file_path, existing).unwrap();
        index.reindex_file(&file_path, "session").unwrap();
        let emb = provider.embed_batch(&[existing]).await.unwrap();
        let chunk_id = format!("{}:0", file_path.to_string_lossy());
        let Some(emb0) = emb.first() else {
            panic!("embed_batch returns one vector per input: {emb:?}");
        };
        index.upsert_embedding(&chunk_id, emb0).unwrap();

        // Different content should not be flagged as duplicate.
        let novel = "## Architecture\n\nThe API uses Python FastAPI with async handlers.";
        let result = is_semantically_duplicate(
            novel,
            &index,
            Some(&provider),
            SEMANTIC_DEDUP_SIMILARITY_THRESHOLD,
        )
        .await;
        assert!(
            !result,
            "different content should not be flagged as duplicate"
        );
    }
}
