// Modified for Distill by Samuel Fajreldines, 2026.
use distill_memory::{
    MemoryObservationSink, MemoryRetrievalMode, MemorySearchErrorClass, MemorySearchObservation,
    MemorySearchSource, MemoryWatcherSyncObservation,
};
use distill_telemetry::memory_telemetry::{
    MemoryInjection, MemoryInjectionOutcome, MemorySearch,
    MemorySearchErrorClass as TelemetryErrorClass, MemorySearchMode as TelemetryMode,
    MemorySearchOutcome as TelemetryOutcome, MemorySearchSource as TelemetrySource,
    MemoryWatcherSync,
};

pub(crate) struct TelemetryMemoryObservationSink {
    pub(crate) session_id: String,
}

#[derive(Default)]
pub(crate) struct MemoryInjectionMetrics {
    pub(crate) is_greeting_fallback: bool,
    pub(crate) result_count: usize,
    pub(crate) total_snippet_chars: usize,
    pub(crate) top_score: f64,
    pub(crate) configured_min_score: f64,
    pub(crate) duration_ms: u64,
    pub(crate) injected_bytes: u64,
    pub(crate) estimated_tokens: u64,
    pub(crate) global_entry_count: usize,
    pub(crate) workspace_entry_count: usize,
    pub(crate) was_reused: bool,
}

pub(crate) fn log_memory_injection(
    session_id: String,
    outcome: MemoryInjectionOutcome,
    metrics: MemoryInjectionMetrics,
) {
    distill_telemetry::session_ctx::log_event(MemoryInjection {
        session_id,
        outcome,
        was_greeting_fallback: metrics.is_greeting_fallback,
        result_count: metrics.result_count,
        total_snippet_chars: metrics.total_snippet_chars,
        top_score: metrics.top_score,
        configured_min_score: metrics.configured_min_score,
        injection_duration_ms: metrics.duration_ms,
        injected_bytes: metrics.injected_bytes,
        estimated_tokens: metrics.estimated_tokens,
        global_entry_count: metrics.global_entry_count,
        workspace_entry_count: metrics.workspace_entry_count,
        was_reused: metrics.was_reused,
    });
}

pub(crate) fn memory_v2_model_usage(
    model: &str,
    response: &distill_sampling_types::ConversationResponse,
) -> distill_telemetry::memory_telemetry::MemoryV2ModelUsage {
    distill_telemetry::memory_telemetry::MemoryV2ModelUsage {
        model_id: Some(model.to_owned()),
        prompt_tokens: response.usage.as_ref().map(|usage| usage.prompt_tokens),
        completion_tokens: response.usage.as_ref().map(|usage| usage.completion_tokens),
        reasoning_tokens: response.usage.as_ref().map(|usage| usage.reasoning_tokens),
        cached_prompt_tokens: response
            .usage
            .as_ref()
            .map(|usage| usage.cached_prompt_tokens),
        cache_creation_tokens: response
            .usage
            .as_ref()
            .map(|usage| usage.cache_creation_prompt_tokens),
        cost_usd_ticks: response.cost_usd_ticks,
    }
}

impl MemoryObservationSink for TelemetryMemoryObservationSink {
    fn observe_search(&self, observation: MemorySearchObservation) {
        distill_telemetry::session_ctx::log_event(MemorySearch {
            session_id: self.session_id.clone(),
            source: match observation.source {
                MemorySearchSource::Tool => TelemetrySource::Tool,
                MemorySearchSource::Injection => TelemetrySource::Injection,
                MemorySearchSource::CompactionRecovery => TelemetrySource::CompactionRecovery,
            },
            mode: match observation.mode {
                MemoryRetrievalMode::FtsOnly => TelemetryMode::FtsOnly,
                MemoryRetrievalMode::Hybrid => TelemetryMode::Hybrid,
                MemoryRetrievalMode::EmbeddingFallback => TelemetryMode::EmbeddingFallback,
            },
            outcome: match observation.outcome {
                distill_memory::MemorySearchOutcome::Results => TelemetryOutcome::Results,
                distill_memory::MemorySearchOutcome::Empty => TelemetryOutcome::Empty,
                distill_memory::MemorySearchOutcome::Error => TelemetryOutcome::Error,
            },
            query_length: observation.query_length,
            keyword_count: observation.keyword_count,
            result_count: observation.result_count,
            top_score: observation.top_score,
            min_score_threshold: observation.min_score_threshold,
            duration_ms: observation.duration_ms,
            vec_available: observation.is_vector_available,
            error_class: observation
                .error_class
                .map(|error_class| match error_class {
                    MemorySearchErrorClass::IndexOpen => TelemetryErrorClass::IndexOpen,
                    MemorySearchErrorClass::Fts => TelemetryErrorClass::Fts,
                    MemorySearchErrorClass::Vector => TelemetryErrorClass::Vector,
                }),
        });
    }

    fn observe_watcher_sync(&self, observation: MemoryWatcherSyncObservation) {
        distill_telemetry::session_ctx::log_event(MemoryWatcherSync {
            session_id: self.session_id.clone(),
            dirty_file_count: observation.dirty_file_count,
            claimed: observation.is_claimed,
            reindexed_count: observation.reindexed_count,
            embedded_count: observation.embedded_count,
            duration_ms: observation.duration_ms,
        });
    }
}
