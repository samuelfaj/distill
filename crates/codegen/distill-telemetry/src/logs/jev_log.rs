// Modified for Distill by Samuel Fajreldines, 2026.
//! Tracing layer for `target: "jev.decision"` → `~/.grok/logs/jev.jsonl`.
//!
//! Enable with `GROK_LOG_JEV=1`. Every decision the Jev seam makes (or defers)
//! is one JSON line: lever, decision, escalated, confidence, model, latency,
//! tokens, and the reason. No request/response body and no credential is ever
//! written here — the seam's own records contain neither.
//!
//! Mirrors [`crate::sampling_log`]: same env-gated layer shape, same target
//! filter, same size cap, so a real session can be audited after the fact.

use std::path::PathBuf;
use std::sync::Mutex;

use tracing::Subscriber;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::Layer;
use tracing_subscriber::registry::LookupSpan;

use distill_config::distill_home;

use crate::instrumentation::{NoOpLayer, TargetFilterLayer};

const ENV_VAR: &str = "GROK_LOG_JEV";
const LOG_FILE: &str = "jev.jsonl";

/// The tracing target the Jev decision records use. The workspace crate emits
/// to this exact string (`crate::jev::policy::TracingSink`); the test below
/// pins it so a rename cannot silently drop every decision on the floor.
pub const TARGET: &str = "jev.decision";

static GUARD: std::sync::OnceLock<Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>> =
    std::sync::OnceLock::new();

/// The env-gated layer installed by the pager and the pager binary.
pub fn layer<S>() -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    if !std::env::var(ENV_VAR).is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "on")) {
        return Box::new(NoOpLayer::new());
    }
    let path = distill_home().join(crate::unified_log::LOG_DIR).join(LOG_FILE);
    layer_for_path(path)
}

/// Builds the layer for an explicit path (the default above, and the tests).
pub fn layer_for_path<S>(path: PathBuf) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'span> LookupSpan<'span> + Send + Sync + 'static,
{
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("failed to create jev log dir: {e}");
        return Box::new(NoOpLayer::new());
    }

    if crate::unified_log::file_size(&path) >= crate::unified_log::MAX_SIZE {
        crate::unified_log::trim_file(&path);
    }

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("failed to open jev log: {e}");
            return Box::new(NoOpLayer::new());
        }
    };

    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    let guard_slot = GUARD.get_or_init(|| Mutex::new(None));
    if let Ok(mut slot) = guard_slot.lock() {
        *slot = Some(guard);
    }

    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .with_ansi(false)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(false)
        .with_writer(BoxMakeWriter::new(non_blocking));

    Box::new(TargetFilterLayer::new(fmt_layer, TARGET))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// The layer keeps one process-wide worker guard, so tests that build a
    /// layer must not run concurrently (each new layer would drop the previous
    /// guard and close the earlier writer mid-test).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_path(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("jev-log-{tag}-{}-{unique}.jsonl", std::process::id()))
    }

    /// Polls the file until `needle` shows up (the appender is non-blocking).
    fn wait_for(path: &std::path::Path, needle: &str, timeout: std::time::Duration) -> String {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let body = std::fs::read_to_string(path).unwrap_or_default();
            if body.contains(needle) || std::time::Instant::now() > deadline {
                return body;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The parsed JSON object of the first line containing `needle`.
    fn line_with(body: &str, needle: &str) -> Option<serde_json::Value> {
        body.lines()
            .filter(|line| line.contains(needle))
            .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    }

    #[test]
    fn the_target_constant_matches_the_seam_contract() {
        // The workspace crate's TracingSink emits to this literal; if either
        // side changes, decisions stop landing in this file silently.
        assert_eq!(TARGET, "jev.decision");
    }

    #[test]
    fn a_decision_event_lands_in_the_file() {
        let _lock = test_guard();
        let path = temp_path("decision");
        let layer = layer_for_path::<tracing_subscriber::Registry>(path.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _default = tracing::subscriber::set_default(subscriber);

        tracing::info!(
            target: "jev.decision",
            lever = "permission_classifier",
            decision = "block",
            escalated = false,
            confidence = 0.97_f64,
            model = "jev-1.13.0",
            latency_ms = 420_u64,
            input_tokens = 1180_u64,
            output_tokens = 181_u64,
            "jev decision"
        );

        let body = wait_for(&path, "\"decision\":\"block\"", std::time::Duration::from_secs(3));
        let parsed =
            line_with(&body, "\"decision\":\"block\"").unwrap_or_else(|| panic!("no line: {body}"));
        assert_eq!(parsed["fields"]["lever"], "permission_classifier");
        assert_eq!(parsed["fields"]["decision"], "block");
        assert_eq!(parsed["fields"]["escalated"], false);
        assert_eq!(parsed["fields"]["model"], "jev-1.13.0");
        assert_eq!(parsed["fields"]["input_tokens"], 1180);
        assert_eq!(parsed["fields"]["output_tokens"], 181);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unrelated_targets_are_filtered_out() {
        let _lock = test_guard();
        let path = temp_path("filter");
        let layer = layer_for_path::<tracing_subscriber::Registry>(path.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _default = tracing::subscriber::set_default(subscriber);

        tracing::info!(target: "some.other.target", "must not be captured");
        // Then a real one, so we can tell "filtered" from "writer never flushed".
        tracing::info!(target: "jev.decision", decision = "escalate", "jev decision");

        let body = wait_for(&path, "\"decision\":\"escalate\"", std::time::Duration::from_secs(3));
        assert!(
            body.contains("escalate"),
            "the targeted event must land: {body}"
        );
        assert!(
            !body.contains("must not be captured"),
            "the other target leaked in: {body}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_log_file_is_appended_not_truncated() {
        let _lock = test_guard();
        let path = temp_path("append");
        std::fs::write(&path, b"{\"pre\":true}\n").expect("seed file");
        let layer = layer_for_path::<tracing_subscriber::Registry>(path.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _default = tracing::subscriber::set_default(subscriber);
        tracing::info!(target: "jev.decision", decision = "allow", "jev decision");
        let body = wait_for(&path, "\"decision\":\"allow\"", std::time::Duration::from_secs(3));
        assert!(
            body.contains("\"pre\":true"),
            "existing lines must survive: {body}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_layer_writes_valid_json_lines() {
        let _lock = test_guard();
        let path = temp_path("json");
        let layer = layer_for_path::<tracing_subscriber::Registry>(path.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _default = tracing::subscriber::set_default(subscriber);
        tracing::info!(target: "jev.decision", decision = "allow", reason = "routine", "jev decision");
        let body = wait_for(&path, "\"decision\":\"allow\"", std::time::Duration::from_secs(3));
        let line = body
            .lines()
            .find(|l| l.contains("\"decision\":\"allow\""))
            .expect("decision line");
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("each line is JSON (spec §12.5 evidence)");
        assert_eq!(parsed["fields"]["reason"], "routine");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_writer_never_sees_a_credential() {
        let _lock = test_guard();
        // Guard-rail: the sink's field set has no credential field, and this
        // test fails if someone adds one that carries a key-shaped value.
        let path = temp_path("nokey");
        let layer = layer_for_path::<tracing_subscriber::Registry>(path.clone());
        let subscriber = tracing_subscriber::registry().with(layer);
        let _default = tracing::subscriber::set_default(subscriber);
        tracing::info!(target: "jev.decision", decision = "allow", "jev decision");
        let body = wait_for(&path, "\"decision\":\"allow\"", std::time::Duration::from_secs(3));
        assert!(
            !body.contains("apikey_"),
            "no key-shaped value may appear: {body}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
