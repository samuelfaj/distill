// Modified for Distill by Samuel Fajreldines, 2026.
//! Ambient session context for telemetry: product events and Mixpanel via [`log_event`].
//! `session_id` and `turn_number` are injected from the task-local [`TelemetryCtx`] active for the duration of a session.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Serialize;
use serde_json::json;

use crate::client::{self, Metadata, UserContext};
use crate::events::TelemetryEvent;

/// `log_event` snapshots this synchronously at call time to avoid racing with turn increments.
#[derive(Clone)]
pub struct TelemetryCtx {
    pub session_id: String,
    pub prompt_index: Arc<tokio::sync::Mutex<usize>>,
    /// Per-prompt correlation UUID for the external OTEL stream (`prompt.id`, events only, never metrics).
    /// Set at turn start where `prompt_index` increments; `None` outside a prompt.
    pub prompt_id: Arc<parking_lot::Mutex<Option<String>>>,
}

impl TelemetryCtx {
    pub fn new(session_id: String, prompt_index: Arc<tokio::sync::Mutex<usize>>) -> Self {
        Self {
            session_id,
            prompt_index,
            prompt_id: Arc::new(parking_lot::Mutex::new(None)),
        }
    }
}

/// Snapshot of the ambient ctx for the external OTEL stream.
pub(crate) struct ExternalCtxSnapshot {
    pub session_id: String,
    pub turn_number: Option<u32>,
    pub prompt_id: Option<String>,
}

/// Rotate the per-prompt correlation UUID at turn start (where `prompt_index` increments). No-op outside a session ctx scope.
/// The id is attached as `prompt.id` to external OTEL events only.
pub fn begin_prompt_id() {
    let _ = TELEMETRY_CTX.try_with(|c| {
        *c.prompt_id.lock() = Some(uuid::Uuid::new_v4().to_string());
    });
}

/// Snapshot the task-local ctx (if any) for external emission.
/// Non-blocking: a contended `prompt_index` lock yields `turn_number = None` rather than stalling the emitting task.
pub(crate) fn external_ctx_snapshot() -> Option<ExternalCtxSnapshot> {
    TELEMETRY_CTX
        .try_with(|c| ExternalCtxSnapshot {
            session_id: c.session_id.clone(),
            turn_number: c.prompt_index.try_lock().map(|g| *g as u32).ok(),
            prompt_id: c.prompt_id.lock().clone(),
        })
        .ok()
}

tokio::task_local! {
    static TELEMETRY_CTX: Arc<TelemetryCtx>;
}

/// The `session_id` field name the debug-log firehose router keys on.
/// `debug_log::SessionIdVisitor` stashes a `SessionId` extension on any span carrying this field; the span *name* plays no part in routing.
/// Shared so the `info_span!` here and the router in `debug_log` can't silently drift; a rename trips `session_span_exposes_router_field` below.
pub(crate) const SESSION_ID_FIELD: &str = "session_id";

/// Build the per-session tracing span the firehose router routes by.
/// The field name MUST be the literal `session_id` (tracing field names can't be a const); the test below pins it against [`SESSION_ID_FIELD`].
fn session_span(session_id: &str) -> tracing::Span {
    tracing::info_span!("session", session_id = %session_id)
}

/// Run `fut` with telemetry context active. Also sets a `tracing` span.
pub async fn with_session_ctx<F: std::future::Future>(ctx: TelemetryCtx, fut: F) -> F::Output {
    use tracing::Instrument;
    let span = session_span(&ctx.session_id);
    TELEMETRY_CTX
        .scope(Arc::new(ctx), fut.instrument(span))
        .await
}

/// Clone the ambient ctx; `None` in a `spawn_local` child, whose task-locals are not inherited, so call this on the parent.
fn clone_current() -> Option<TelemetryCtx> {
    TELEMETRY_CTX.try_with(|c| (**c).clone()).ok()
}

/// `spawn_local` `fut` with the caller's [`TelemetryCtx`] re-entered in the child, so turn work keeps `session.id`/`prompt.id`/`turn_number`.
pub fn spawn_local_in_session_ctx<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + 'static,
    F::Output: 'static,
{
    let ctx = clone_current();
    tokio::task::spawn_local(async move {
        match ctx {
            Some(ctx) => with_session_ctx(ctx, fut).await,
            None => fut.await,
        }
    })
}

/// Selects the analytics event-name prefix so shell and workspace events are distinguishable on the wire.
/// Both origins share this emitter and the `event_value` derivation in [`crate::client`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumCount)]
pub enum EmitterOrigin {
    /// `distill-shell` (and the pager/TUI that emit through it).
    Shell,
    /// `distill-workspace` (remote sampler / workspace server).
    Workspace,
}

impl EmitterOrigin {
    /// [`crate::client::event_value`] iterates this to strip the prefix an event name carries; the `const _` assertion below keeps it complete.
    /// The prefixes are mutually exclusive (pinned by `client`'s `emitter_prefixes_are_mutually_exclusive` test), so at most one entry matches.
    pub const ALL: [EmitterOrigin; 2] = [EmitterOrigin::Shell, EmitterOrigin::Workspace];

    /// Analytics event-name prefix for this origin.
    /// [`crate::client::event_value`] strips the same prefix to derive the wire `event_value`, so the two must stay in lockstep.
    pub fn event_prefix(self) -> &'static str {
        match self {
            EmitterOrigin::Shell => "grok-shell-",
            EmitterOrigin::Workspace => "grok-workspace-",
        }
    }
}

/// A variant missing from [`EmitterOrigin::ALL`] makes `ALL.len()` diverge from the `strum::EnumCount` count and trips this assertion.
/// That keeps `client::event_value` from silently skipping a new origin's prefix.
const _: () = assert!(EmitterOrigin::ALL.len() == <EmitterOrigin as strum::EnumCount>::COUNT);

/// Product analytics event (type-safe). Only fires in `Enabled` mode.
/// Unconditionally fans out to the external OTEL stream first; that gate is `external::is_active()`, independent of `TelemetryMode`.
pub fn log_event<T: TelemetryEvent>(data: T) {
    crate::external::emit(&data);
    if !client::is_enabled() {
        return;
    }
    emit_event(T::NAME, data);
}

/// Like [`log_event`], but awaits delivery on the current runtime.
/// Fire-and-forget posts die with the session runtime on pager/embedded `/exit`, where [`drain_at_session_exit`] is a no-op.
/// The process-exit drain cannot see this runtime either. Use for the last emit on that path.
pub async fn log_event_now<T: TelemetryEvent>(data: T) {
    crate::external::emit(&data);
    if !client::is_enabled() {
        return;
    }
    emit_event_now(T::NAME, data).await;
}

/// Emit one event to the external stream always and to the product events/Mixpanel funnel only when `internal_enabled`.
/// Callers use this when their internal sink is gated more strictly than [`log_event`]'s `Enabled` check (the shell's `Enabled && !ZDR`).
/// [`log_event`] already fans out externally, so the branch keeps the external emit exactly-once and never sends an internal record under ZDR.
pub fn log_event_dual<T: TelemetryEvent>(internal_enabled: bool, data: T) {
    if internal_enabled {
        log_event(data);
    } else {
        crate::external::emit(&data);
    }
}

/// Session lifecycle event (type-safe). Fires in both `Enabled` and `SessionMetrics` modes.
/// Emits with the [`EmitterOrigin::Shell`] prefix; workspace-side callers use [`log_session_event_with_origin`].
/// Unconditionally fans out to the external OTEL stream first (independent gate; see [`log_event`]).
pub fn log_session_event<T: TelemetryEvent>(data: T) {
    crate::external::emit(&data);
    if !client::is_session_metrics_enabled() {
        return;
    }
    emit_event_with_origin(EmitterOrigin::Shell, T::NAME, data);
}

/// Session lifecycle event tagged with the emitting [`EmitterOrigin`]. No external fan-out here: the external stream is
/// Shell-origin only, and workspace-side callers invoke this directly. An `external = …` macro arm on a workspace-only
/// event therefore has no effect (pinned by a test in `external::tests`).
pub fn log_session_event_with_origin<T: TelemetryEvent>(origin: EmitterOrigin, data: T) {
    if !client::is_session_metrics_enabled() {
        return;
    }
    emit_event_with_origin(origin, T::NAME, data);
}

/// Emit an event with the default [`EmitterOrigin::Shell`] prefix.
pub fn emit_event<T: Serialize + Send + 'static>(event_suffix: impl Into<String>, data: T) {
    emit_event_with_origin(EmitterOrigin::Shell, event_suffix, data);
}

/// Await delivery of a Shell-origin event on the current runtime.
pub async fn emit_event_now<T: Serialize + Send + 'static>(
    event_suffix: impl Into<String>,
    data: T,
) {
    emit_event_with_origin_now(EmitterOrigin::Shell, event_suffix, data).await;
}

/// Posts spawned by [`emit_event_with_origin`] that haven't finished.
/// Emission is fire-and-forget so it never blocks a turn; a process exiting right after emitting drops the event (see [`drain_pending`]).
static PENDING_EVENTS: AtomicUsize = AtomicUsize::new(0);

/// Decrement on every exit path, including a panicking, cancelled, or never-polled post.
struct PendingEventGuard;

impl PendingEventGuard {
    fn register() -> Self {
        PENDING_EVENTS.fetch_add(1, Ordering::Release);
        Self
    }
}

impl Drop for PendingEventGuard {
    fn drop(&mut self) {
        PENDING_EVENTS.fetch_sub(1, Ordering::Release);
    }
}

/// Drain bound for one-shot CLI commands. Returns once the post lands (~1.7s cold); the bound only bites on a black-holed network.
pub const CLI_DRAIN: std::time::Duration = std::time::Duration::from_secs(5);

const SESSION_EXIT_DRAIN: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) fn drains_at_session_exit(entrypoint: Option<crate::process_info::Entrypoint>) -> bool {
    use crate::process_info::Entrypoint;
    matches!(
        entrypoint,
        None | Some(Entrypoint::Headless | Entrypoint::Cli)
    )
}

/// Session end is process end only for one-shot flows; every other process drains at [`drain_at_process_exit`].
pub async fn drain_at_session_exit() {
    if drains_at_session_exit(crate::process_info::entrypoint()) {
        drain_pending(SESSION_EXIT_DRAIN).await;
    }
}

pub async fn drain_at_process_exit() {
    drain_pending(SESSION_EXIT_DRAIN).await;
}

/// Wait (up to `timeout`) for in-flight event posts to finish.
/// Meant for commands that exit as soon as their work is done; the agent runs long enough that its events land on their own.
pub async fn drain_pending(timeout: std::time::Duration) {
    let drain_span = crate::region::Region::from_span(tracing::info_span!(
        "teardown.telemetry_drain",
        pending = PENDING_EVENTS.load(Ordering::Acquire) as i64,
        elapsed_ms = tracing::field::Empty,
    ));
    let started = std::time::Instant::now();
    let deadline = std::time::Instant::now() + timeout;
    while PENDING_EVENTS.load(Ordering::Acquire) > 0 {
        if std::time::Instant::now() >= deadline {
            tracing::debug!(
                pending = PENDING_EVENTS.load(Ordering::Acquire),
                "telemetry: gave up draining pending events"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    drain_span
        .span()
        .record("elapsed_ms", started.elapsed().as_millis() as i64);
}

type CtxSnapshot = Option<(String, Option<u32>)>;

fn take_emit_context<T>(
    origin: EmitterOrigin,
    event_suffix: impl Into<String>,
    data: T,
) -> (String, CtxSnapshot, crate::activity::ActivitySnapshot, T) {
    let event_name = format!("{}{}", origin.event_prefix(), event_suffix.into());
    let ctx_snapshot = TELEMETRY_CTX
        .try_with(|c| {
            (
                c.session_id.clone(),
                c.prompt_index.try_lock().map(|g| *g as u32).ok(),
            )
        })
        .ok();
    // Read here, not in the spawned post, so an event emitted at a boundary captures activity at emit time
    let activity = crate::activity::ActivitySnapshot::read();
    (event_name, ctx_snapshot, activity, data)
}

async fn post_event<T: Serialize>(
    event_name: String,
    ctx_snapshot: CtxSnapshot,
    activity: crate::activity::ActivitySnapshot,
    data: T,
) {
    let user_ctx = UserContext::collect();
    let request_id = format!("{}-{}", event_name, uuid::Uuid::new_v4());

    let mut metadata = match serde_json::to_value(data) {
        Ok(serde_json::Value::Object(map)) => map,
        Ok(other) => {
            let mut m = Metadata::new();
            m.insert("value".into(), other);
            m
        }
        Err(_) => Metadata::new(),
    };

    if let Some((session_id, turn_number)) = ctx_snapshot {
        metadata.insert("session_id".into(), json!(session_id));
        if let Some(turn) = turn_number {
            metadata.insert("turn_number".into(), json!(turn));
        }
    }

    if let Ok(serde_json::Value::Object(gauges)) = serde_json::to_value(activity) {
        for (key, value) in gauges {
            metadata.entry(key).or_insert(value);
        }
    }

    crate::redact_common::redact_metadata(&mut metadata);
    client::track(&event_name, &request_id, &user_ctx, metadata).await;
}

/// Emit an event whose analytics name is `{origin prefix}{event_suffix}`.
pub fn emit_event_with_origin<T: Serialize + Send + 'static>(
    origin: EmitterOrigin,
    event_suffix: impl Into<String>,
    data: T,
) {
    let (event_name, ctx_snapshot, activity, data) = take_emit_context(origin, event_suffix, data);

    // `tokio::spawn` panics without a runtime. Skip before register so PENDING_EVENTS
    // is not pinned above zero for the rest of the process.
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::debug!(event = %event_name, "telemetry: no runtime, dropping event");
        return;
    }
    let pending = PendingEventGuard::register();
    tokio::spawn(async move {
        let _pending = pending;
        post_event(event_name, ctx_snapshot, activity, data).await;
    });
}

/// Await delivery on the current runtime.
/// Does not register [`PENDING_EVENTS`]: the caller blocks until the post finishes, so a following runtime drop cannot abort it.
pub async fn emit_event_with_origin_now<T: Serialize + Send + 'static>(
    origin: EmitterOrigin,
    event_suffix: impl Into<String>,
    data: T,
) {
    let (event_name, ctx_snapshot, activity, data) = take_emit_context(origin, event_suffix, data);
    post_event(event_name, ctx_snapshot, activity, data).await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_one_shot_flows_drain_at_session_exit() {
        use crate::process_info::Entrypoint;
        use crate::session_ctx::drains_at_session_exit;
        assert!(drains_at_session_exit(None), "undeclared stays fail-open");
        assert!(drains_at_session_exit(Some(Entrypoint::Headless)));
        assert!(drains_at_session_exit(Some(Entrypoint::Cli)));
        for outlives in [
            Entrypoint::Embedded,
            Entrypoint::Pager,
            Entrypoint::Leader,
            Entrypoint::Workspace,
        ] {
            assert!(
                !drains_at_session_exit(Some(outlives)),
                "{outlives:?} outlives its sessions and must not block teardown"
            );
        }
    }

    use super::*;

    /// The debug-log firehose router (`debug_log`) finds the session span by its `session_id` field (not by name).
    /// That field name is a literal in `session_span` (tracing field names can't be a const), so pin it against the shared const here.
    /// A rename of either breaks this test instead of silently degrading routing to the per-pid fallback.
    #[test]
    fn session_span_exposes_router_field() {
        // A bare registry enables every callsite, so the span has live metadata.
        let subscriber = tracing_subscriber::registry();
        tracing::subscriber::with_default(subscriber, || {
            let span = session_span("test-id");
            let meta = span
                .metadata()
                .expect("session span must have metadata under an enabling subscriber");
            assert!(
                meta.fields().field(SESSION_ID_FIELD).is_some(),
                "session span must expose `{SESSION_ID_FIELD}` for debug-log routing",
            );
        });
    }

    /// Serializes tests that assert on the process-global `PENDING_EVENTS` gauge.
    static EMIT_TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Regression: the awaited emit must finish without the session-exit drain.
    #[tokio::test]
    async fn awaited_emit_finishes_without_session_exit_drain() {
        let _guard = EMIT_TEST_GUARD.lock().await;
        let before = PENDING_EVENTS.load(Ordering::Acquire);
        emit_event_with_origin_now(
            EmitterOrigin::Shell,
            "session_end_timings",
            json!({ "probe": true }),
        )
        .await;
        assert_eq!(
            PENDING_EVENTS.load(Ordering::Acquire),
            before,
            "awaited emit must not register a PENDING_EVENTS guard (survives runtime drop)"
        );
    }

    /// What a command exiting right after emitting (`grok login`) relies on.
    /// Asserts on the wait, not on the gauge: it is process-global and other tests in this binary emit concurrently.
    #[tokio::test]
    async fn drain_pending_waits_for_in_flight_posts() {
        let _guard = EMIT_TEST_GUARD.lock().await;
        emit_event_with_origin(
            EmitterOrigin::Shell,
            "drain_probe",
            json!({ "probe": true }),
        );
        assert!(
            PENDING_EVENTS.load(Ordering::Acquire) > 0,
            "emission must register before the post is awaited"
        );

        let started = std::time::Instant::now();
        let budget = std::time::Duration::from_secs(5);
        drain_pending(budget).await;
        assert!(
            started.elapsed() < budget,
            "drain must observe the post finish, not time out"
        );
    }

    /// `tokio::spawn` panics without a runtime; the session-metrics gate is process-global.
    #[test]
    fn emit_event_with_origin_is_noop_without_runtime() {
        assert!(tokio::runtime::Handle::try_current().is_err());
        emit_event_with_origin(EmitterOrigin::Shell, "regression-no-runtime", ());
    }

    /// Event-name prefixes are wire contract: analytics queries match on them, so they must not drift.
    #[test]
    fn event_prefix_is_stable_per_origin() {
        assert_eq!(EmitterOrigin::Shell.event_prefix(), "grok-shell-");
        assert_eq!(EmitterOrigin::Workspace.event_prefix(), "grok-workspace-");
    }

    /// The `Shell` reroute must reproduce the historical `format!("grok-shell-{suffix}")` event name byte-for-byte.
    /// Every existing `log_session_event` / `log_event` / `emit_event` call funnels through `EmitterOrigin::Shell`.
    #[test]
    fn shell_origin_event_name_matches_legacy_format() {
        let suffix = "trace_upload_attempted";
        let rerouted = format!("{}{}", EmitterOrigin::Shell.event_prefix(), suffix);
        let legacy = format!("grok-shell-{suffix}");
        assert_eq!(rerouted, legacy);
    }

    #[test]
    fn workspace_origin_event_name_uses_workspace_prefix() {
        let name = format!("{}turn", EmitterOrigin::Workspace.event_prefix());
        assert_eq!(name, "grok-workspace-turn");
    }

    /// `ALL` must enumerate every variant so the stripper in `client` can recover the `event_value` for any origin the emitter produces.
    /// Length completeness is also compiler-enforced by the `const _` assertion in this module (via `strum::EnumCount`).
    /// This test additionally pins the known variants and that every prefix is distinct and non-empty, which `EnumCount` alone does not guarantee.
    #[test]
    fn all_covers_every_origin_with_distinct_nonempty_prefixes() {
        assert!(EmitterOrigin::ALL.contains(&EmitterOrigin::Shell));
        assert!(EmitterOrigin::ALL.contains(&EmitterOrigin::Workspace));
        assert_eq!(
            EmitterOrigin::ALL.len(),
            <EmitterOrigin as strum::EnumCount>::COUNT,
            "ALL must list every EmitterOrigin variant",
        );

        let mut prefixes: Vec<&str> = EmitterOrigin::ALL
            .iter()
            .map(|o| o.event_prefix())
            .collect();
        assert!(
            prefixes.iter().all(|p| !p.is_empty()),
            "every origin must have a non-empty prefix",
        );
        let total = prefixes.len();
        prefixes.sort_unstable();
        prefixes.dedup();
        assert_eq!(
            prefixes.len(),
            total,
            "every origin must yield a distinct prefix",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_local_in_session_ctx_reenters_parent_ctx() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ctx = TelemetryCtx::new(
                    "sess-inherit".into(),
                    Arc::new(tokio::sync::Mutex::new(7usize)),
                );
                with_session_ctx(ctx, async {
                    begin_prompt_id();
                    let parent_prompt = external_ctx_snapshot().expect("parent ctx").prompt_id;
                    assert!(parent_prompt.is_some());
                    let child = spawn_local_in_session_ctx(async {
                        external_ctx_snapshot().expect("child re-enters ctx")
                    })
                    .await
                    .expect("join");
                    assert_eq!(child.session_id, "sess-inherit");
                    assert_eq!(child.turn_number, Some(7));
                    assert_eq!(child.prompt_id, parent_prompt);
                })
                .await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_local_in_session_ctx_is_noop_outside_ctx() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let seen = spawn_local_in_session_ctx(async { external_ctx_snapshot().is_some() })
                    .await
                    .expect("join");
                assert!(!seen, "helper must not invent a ctx");
            })
            .await;
    }

    /// A `TurnCompleted` with no event-carried `session_id` carries one only via the ambient ctx.
    /// The id is present via the helper and absent from a bare child.
    #[tokio::test(flavor = "current_thread")]
    async fn spawn_local_in_session_ctx_puts_session_id_on_work_event() {
        use crate::external::test_support::{TestStream, build, emit_event_into};
        use opentelemetry::logs::AnyValue;

        fn work() -> crate::events::TurnCompleted {
            crate::events::TurnCompleted {
                outcome: crate::events::Outcome::Completed,
                duration_ms: 10,
                tool_call_count: 1,
                model_id: "grok-4".into(),
                // Left `None` so this exercises the task-local ctx fallback, not the event field.
                session_id: None,
                cancellation_category: None,
                error_category: None,
                error_code: None,
                error_detail: None,
                context_tokens: None,
                turn_tokens: None,
            }
        }
        fn session_id(stream: &TestStream) -> Option<String> {
            let logs = stream.logs.get_emitted_logs().expect("in-memory logs");
            logs.first()?.record.attributes_iter().find_map(|(k, v)| {
                (k.as_str() == "session.id").then(|| match v {
                    AnyValue::String(s) => s.as_str().to_owned(),
                    other => format!("{other:?}"),
                })
            })
        }

        tokio::task::LocalSet::new()
            .run_until(async {
                let ctx = TelemetryCtx::new(
                    "sess-wire".into(),
                    Arc::new(tokio::sync::Mutex::new(1usize)),
                );
                with_session_ctx(ctx, async {
                    let bare = tokio::task::spawn_local(async {
                        let s = build(Default::default());
                        emit_event_into(&s, &work());
                        session_id(&s)
                    })
                    .await
                    .expect("join");
                    assert_eq!(bare, None, "bare spawn_local drops session.id");

                    let fixed = spawn_local_in_session_ctx(async {
                        let s = build(Default::default());
                        emit_event_into(&s, &work());
                        session_id(&s)
                    })
                    .await
                    .expect("join");
                    assert_eq!(fixed.as_deref(), Some("sess-wire"));
                })
                .await;
            })
            .await;
    }
}
