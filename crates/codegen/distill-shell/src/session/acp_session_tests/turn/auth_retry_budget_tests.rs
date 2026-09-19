// Modified for Distill by Samuel Fajreldines, 2026.
//! These tests run the real turn loop against a mock server that 401s unauthenticated requests and 200s a fresh bearer.
//! A fail-closed (credential-less) 401 must not consume `AuthRetrySchedule` budget.
//! That was the failure seen in the field: each sleep cycle burned one slot.
//! Credentialed 401s must still exhaust after `MAX_RETRIES`.

use super::support::*;
use super::*;
use distill_login::{AuthManager, AuthMode, GrokAuth, GrokComConfig};
use distill_test_support::{MockInferenceServer, MockModelEntry, ScriptedResponse};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// The token the mock server accepts and the refresher mints on success.
const FRESH_TOKEN: &str = "refreshed-test-token";

/// With `fail_pre_request`, mimics the post-wake sequence: `PreRequest` refreshes fail (the send
/// goes out fail-closed) while 401 recovery mints [`FRESH_TOKEN`] for `mint_ttl` — a TTL inside
/// the pre-request buffer (< 5 min) stays wire-valid yet keeps later prepares observable in `calls`.
struct WakeGapRefresher {
    calls: Arc<AtomicU32>,
    fail_pre_request: bool,
    mint_ttl: chrono::Duration,
}

#[async_trait::async_trait]
impl distill_login::refresh::TokenRefresher for WakeGapRefresher {
    async fn refresh(
        &self,
        reason: distill_login::refresh::RefreshReason,
    ) -> distill_login::refresh::RefreshOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_pre_request && reason == distill_login::refresh::RefreshReason::PreRequest {
            return distill_login::refresh::RefreshOutcome::TransientFailure {
                message: "simulated post-wake network gap".to_string(),
            };
        }
        distill_login::refresh::RefreshOutcome::success(GrokAuth {
            key: FRESH_TOKEN.to_string(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(chrono::Utc::now() + self.mint_ttl),
            ..GrokAuth::test_default()
        })
    }
}

/// `(tempdir, manager)` with a hard-expired OIDC token, so the wire-valid resolver has nothing to stamp until the refresher succeeds.
/// The tempdir must outlive the manager (auth.json path).
fn expired_auth_manager(
    refresher: Arc<dyn distill_login::refresh::TokenRefresher>,
) -> (tempfile::TempDir, Arc<AuthManager>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
    am.hot_swap(GrokAuth {
        key: "initial-test-key".into(),
        auth_mode: AuthMode::Oidc,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
        ..GrokAuth::test_default()
    });
    am.set_refresher(refresher);
    (dir, am)
}

/// `x.ai/session_notification` payloads the client was sent.
type XaiUpdates = Arc<parking_lot::Mutex<Vec<serde_json::Value>>>;

fn drain_gateway(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
) -> XaiUpdates {
    let captured = XaiUpdates::default();
    let sink = captured.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    let _ = args.response_tx.send(Ok(()));
                }
                distill_acp_lib::AcpClientMessage::ExtNotification(args) => {
                    if let Ok(value) = serde_json::from_str(args.params.get()) {
                        sink.lock().push(value);
                    }
                }
                _ => {}
            }
        }
    });
    captured
}

/// `(error_type, message)` of the turn's terminal `retryState`, if the client was told about one.
fn terminal_failure(updates: &XaiUpdates) -> Option<(String, String)> {
    updates.lock().iter().find_map(|value| {
        let update = value.get("update")?;
        if update.get("sessionUpdate")? != "retry_state" || update.get("type")? != "failed" {
            return None;
        }
        Some((
            update.get("error_type")?.as_str()?.to_owned(),
            update.get("message")?.as_str()?.to_owned(),
        ))
    })
}

/// `(attempt, max_retries, reason)` of every `retryState` Retrying update the
/// client was sent, in order.
fn retrying_updates(updates: &XaiUpdates) -> Vec<(u32, u32, String)> {
    updates
        .lock()
        .iter()
        .filter_map(|value| {
            let update = value.get("update")?;
            if update.get("sessionUpdate")? != "retry_state" || update.get("type")? != "retrying" {
                return None;
            }
            Some((
                update.get("attempt")?.as_u64()? as u32,
                update.get("max_retries")?.as_u64()? as u32,
                update.get("reason")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

/// Assert the full Retrying stream: attempts count 1..=`expected_len`, each carrying
/// `expected_max` and every reason needle (the retry-copy pin).
fn assert_retrying(
    updates: &XaiUpdates,
    expected_len: usize,
    expected_max: u32,
    reason_needles: &[&str],
) {
    let retrying = retrying_updates(updates);
    assert_eq!(
        retrying.len(),
        expected_len,
        "Retrying update count: {retrying:?}"
    );
    for (i, (attempt, max_retries, reason)) in retrying.iter().enumerate() {
        assert_eq!(
            *attempt,
            (i + 1) as u32,
            "attempts must count consecutively"
        );
        assert_eq!(
            *max_retries, expected_max,
            "every Retrying must carry this path's budget: {retrying:?}"
        );
        for needle in reason_needles {
            assert!(reason.contains(needle), "retry copy regressed: {reason}");
        }
    }
}

fn drain_persistence(mut rx: tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>) {
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = msg {
                let _ = respond_to.send(Ok(()));
            }
        }
    });
}

/// Shape options for [`session_token_actor`].
#[derive(Default)]
struct ActorShape {
    /// Shape the actor like a spawned subagent turn — the only shape that
    /// gets a 429 wait budget.
    is_subagent: bool,
    /// Budgeted workflow child (`task_output_token_budget` grant) — excluded from the park.
    task_output_budget: Option<u64>,
    /// Flip the `uncharged_401_park` kill switch off.
    park_disabled: bool,
}

/// Actor wired for session-token auth against the mock server: a real sampler, the `cached_token` method, and the supplied auth manager.
/// `NotByok` model facts keep the session-token gate active against the loopback URL.
async fn session_token_actor(
    server: &MockInferenceServer,
    auth_manager: Arc<AuthManager>,
    shape: ActorShape,
) -> (Arc<SessionActor>, XaiUpdates) {
    let sampling_cfg = distill_sampler::SamplerConfig {
        base_url: server.url(),
        model: "test".to_string(),
        api_backend: distill_sampler::ApiBackend::Responses,
        context_window: 256_000,
        max_retries: Some(0),
        idle_timeout_secs: Some(30),
        ..Default::default()
    };
    let (sampler_event_tx, sampler_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_sampler::SamplingEvent>();
    let sampler_handle = distill_sampler::SamplerActor::spawn(
        sampling_cfg,
        distill_sampler::RetryPolicy {
            max_retries: 0,
            ..Default::default()
        },
        sampler_event_tx,
    );

    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let xai_updates = drain_gateway(gateway_rx);
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    drain_persistence(persistence_rx);

    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.sampler_handle = sampler_handle;
    actor.auth_manager = Some(auth_manager);
    actor.auth_method_id = test_auth_method_id("cached_token");
    if shape.is_subagent {
        let mut hints = actor.startup_hints.clone();
        hints.is_subagent = true;
        actor.startup_hints = hints;
    }
    if shape.park_disabled {
        actor.uncharged_401_park_enabled = false;
    }
    if let Some(grant) = shape.task_output_budget {
        actor.tool_context.task_output_token_budget = Some(
            crate::tools::tool_context::TaskOutputTokenBudget::limited(grant),
        );
    }

    let mut cfg = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor has sampling config");
    cfg.base_url = server.url();
    cfg.api_backend = distill_sampling_types::ApiBackend::Responses;
    cfg.model = "test".to_string();
    actor.chat_state_handle.update_sampling_config(cfg);
    let mut creds = actor.chat_state_handle.get_credentials().await;
    creds.api_key = None;
    creds.auth_type = distill_chat_state::AuthType::SessionToken;
    actor.chat_state_handle.update_credentials(creds);

    // Definite NotByok: the session-token gate must stay active against the loopback mock URL (an `Unknown` would demand a first-party host)
    actor
        .model_auth_memo
        .replace(Some(crate::session::acp_session::ModelAuthMemo {
            model_id: "test".to_string(),
            facts: crate::agent::config::ModelAuthFacts {
                byok: crate::agent::auth_method::ModelByok::NotByok,
                auth_scheme: Default::default(),
            },
            provider: None,
        }));

    actor
        .workspace_ops
        .bind_local_session(
            &actor.session_id_string(),
            actor.tool_context.cwd.as_path().to_path_buf(),
            actor.tool_context.hunk_tracker_handle.clone(),
            actor.agent.borrow().tool_bridge().toolset(),
            None,
        )
        .expect("bind_local_session");

    let actor = Arc::new(actor);
    {
        let drainer = actor.clone();
        let mut sampler_event_rx = sampler_event_rx;
        tokio::task::spawn_local(async move {
            while let Some(event) = sampler_event_rx.recv().await {
                drainer.handle_sampling_event(event).await;
            }
        });
    }
    (actor, xai_updates)
}

async fn run_prompt(
    actor: &Arc<SessionActor>,
    prompt_id: &str,
) -> Result<crate::session::commands::PromptTurnOk, acp::Error> {
    run_prompt_with_cap(actor, prompt_id, 60).await
}

/// [`run_prompt`] with an explicit virtual-time cap: under `start_paused` virtual spend scales
/// with host load, so caps are anti-hang bounds only and never assert timing.
async fn run_prompt_with_cap(
    actor: &Arc<SessionActor>,
    prompt_id: &str,
    cap_secs: u64,
) -> Result<crate::session::commands::PromptTurnOk, acp::Error> {
    let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "hello".to_string(),
    ))];
    // Hang guard, not a latency assertion: only a wedged turn reaches it
    // The exhaustion test runs on a paused clock, and the 401 ladder plus refresh waits burn far past any real-time budget in virtual time
    // Auto-advance makes that virtual time free
    tokio::time::timeout(
        Duration::from_secs(cap_secs),
        actor.handle_prompt(
            prompt_id,
            prompt_blocks,
            PromptMode::Agent,
            None,
            None,
            None,
            None,
            true,
            /* send_now */ false,
            None,
            None,
            None,
        ),
    )
    .await
    .expect("turn must finish within timeout")
}

/// The turn future needs a session-sized stack (spawn.rs: 8 MiB); default test stacks overflow.
fn on_session_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(test)
        .expect("spawn test thread")
        .join()
        .expect("test thread panicked");
}

fn run_current_thread<F: std::future::Future>(paused: bool, fut: impl FnOnce() -> F) {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all();
    if paused {
        builder.start_paused(true);
    }
    let rt = builder.build().expect("test runtime");
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        fut().await;
    }));
}

/// The wake sequence: the resolver has nothing wire-valid, so the send goes out with no `Authorization` header and the server 401s it.
/// Recovery lands a fresh token; the turn must survive and resubmit with the fresh bearer.
#[test]
fn fail_closed_401_is_uncharged_and_turn_survives() {
    on_session_stack(|| {
        run_current_thread(false, || async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                FRESH_TOKEN,
            )
            .await
            .expect("mock inference server");

            let calls = Arc::new(AtomicU32::new(0));
            // Pre-send refreshes fail like a post-wake network gap, so the first send goes out fail-closed; the 401-recovery refresh succeeds
            let refresher = Arc::new(WakeGapRefresher {
                calls: calls.clone(),
                fail_pre_request: true,
                mint_ttl: chrono::Duration::hours(1),
            });
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, updates) = session_token_actor(&server, am, ActorShape::default()).await;

            let outcome = run_prompt(&actor, "auth-retry-budget-fail-closed").await;
            assert!(
                outcome.is_ok(),
                "fail-closed 401 must not fail the turn: {outcome:?}"
            );

            let inference: Vec<_> = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .collect();
            assert!(
                inference.len() >= 2,
                "expected the fail-closed send plus the resubmit; got {}",
                inference.len()
            );
            let Some(first) = inference.first() else {
                panic!("expected inference requests: {inference:?}");
            };
            assert_eq!(
                first.authorization, None,
                "first send must carry no Authorization header"
            );
            assert_eq!(
                inference.last().unwrap().authorization.as_deref(),
                Some(&format!("Bearer {FRESH_TOKEN}") as &str),
                "resubmit must carry the freshly refreshed bearer"
            );
            assert!(
                calls.load(Ordering::SeqCst) >= 2,
                "both the failing pre-flight and the recovery refresh must run"
            );
            // The named invariant: the fail-closed 401 rode the uncharged budget, not a charged slot.
            assert_retrying(
                &updates,
                1,
                AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS,
                &["carried no credential"],
            );
        });
    });
}

/// Rule: kill switch off, a recovered fail-closed 401 resubmits un-parked (no
/// prepare/preflight skips) — full pre-park behavior on rollback.
#[tokio::test(flavor = "current_thread")]
async fn park_disabled_recovered_401_still_resubmits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                FRESH_TOKEN,
            )
            .await
            .expect("mock inference server");
            let calls = Arc::new(AtomicU32::new(0));
            // The minted TTL sits inside the pre-request buffer: wire-valid for the
            // resubmit, yet every refresh-driving prepare stays observable in `calls`.
            let refresher = Arc::new(WakeGapRefresher {
                calls: calls.clone(),
                fail_pre_request: true,
                mint_ttl: chrono::Duration::minutes(4),
            });
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, updates) = session_token_actor(
                &server,
                am,
                ActorShape {
                    park_disabled: true,
                    ..Default::default()
                },
            )
            .await;

            let outcome = run_prompt(&actor, "park-disabled-recovered").await;
            assert!(
                outcome.is_ok(),
                "flag off must not break recovered fail-closed 401s: {outcome:?}"
            );
            let inference: Vec<_> = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .collect();
            assert_eq!(
                inference
                    .last()
                    .expect("at least one send")
                    .authorization
                    .as_deref(),
                Some(&format!("Bearer {FRESH_TOKEN}") as &str),
                "resubmit must carry the recovered bearer"
            );
            assert!(
                terminal_failure(&updates).is_none(),
                "a surviving turn must not report a terminal retryState"
            );
            // Mirror of the flag-on `== 4` pin, exact by design: dropping the
            // kill-switch conjunct from `turn_parked` shows up as missing calls.
            assert_eq!(
                calls.load(Ordering::SeqCst),
                10,
                "flag off must restore every pre-park refresh attempt"
            );
        })
        .await;
}

/// Real credential rejections must still terminate: the escalating budget exhausts after `MAX_RETRIES` when every request carries a rejected bearer.
/// The failure names authenticated rejections, not a generic budget message.
/// `start_paused` auto-advances the backoff ladder.
#[test]
fn authenticated_401s_still_exhaust_after_three_retries() {
    on_session_stack(|| {
        run_current_thread(true, || async {
            // The server only accepts a token the refresher never mints, so every authenticated send is rejected
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                "never-issued-token",
            )
            .await
            .expect("mock inference server");

            let refresher = Arc::new(WakeGapRefresher {
                calls: Arc::new(AtomicU32::new(0)),
                fail_pre_request: false,
                mint_ttl: chrono::Duration::hours(1),
            });
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, updates) = session_token_actor(&server, am, ActorShape::default()).await;

            let outcome = run_prompt_with_cap(&actor, "auth-retry-budget-exhaust", 86_400).await;
            let err = outcome.expect_err("authenticated 401s must exhaust and fail the turn");
            let rendered = serde_json::to_string(&err.data).unwrap_or_default();
            assert!(
                rendered.contains("authenticated inference requests were still rejected"),
                "exhaustion must name authenticated rejections, got: {rendered}"
            );

            let authenticated = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .filter(|r| r.authorization.as_deref() == Some(&format!("Bearer {FRESH_TOKEN}")))
                .count();
            assert_eq!(
                authenticated,
                (AuthRetrySchedule::MAX_RETRIES + 1) as usize,
                "initial send plus MAX_RETRIES resubmits, all authenticated"
            );

            // The budget is the one terminal path outside `handle_sampling_failure`, and it used to return its error with no notification
            // That left the pager with no re-auth prompt and no turn-failed block for a turn that died on 401s
            let (error_type, message) = terminal_failure(&updates)
                .expect("an exhausted turn must report a terminal retryState");
            assert_eq!(
                error_type, "auth",
                "the client keys its re-auth prompt off this: {message}"
            );
            assert!(
                message.contains("authenticated inference requests were still rejected"),
                "the notification must carry the same story as the error: {message}"
            );
            // Inverse of the runaway test's leak guard: "attempt 2/50" here is the bug.
            assert_retrying(
                &updates,
                AuthRetrySchedule::MAX_RETRIES as usize,
                AuthRetrySchedule::MAX_RETRIES,
                &["Re-authenticated after 401"],
            );
        });
    });
}

/// Refresh-outage refresher: every refresh fails transiently, counting `ServerRejected` and
/// `PreRequest` attempts so tests pin that parked cycles dispatch neither. Recovery arrives
/// out-of-band (hot_swap + notify), the way production recovers.
#[derive(Default)]
struct DeferredRefreshNeverLands {
    server_rejected_calls: Arc<AtomicU32>,
    pre_request_calls: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl distill_login::refresh::TokenRefresher for DeferredRefreshNeverLands {
    async fn refresh(
        &self,
        reason: distill_login::refresh::RefreshReason,
    ) -> distill_login::refresh::RefreshOutcome {
        match reason {
            distill_login::refresh::RefreshReason::ServerRejected => {
                self.server_rejected_calls.fetch_add(1, Ordering::SeqCst);
            }
            distill_login::refresh::RefreshReason::PreRequest => {
                self.pre_request_calls.fetch_add(1, Ordering::SeqCst);
            }
        }
        distill_login::refresh::RefreshOutcome::TransientFailure {
            message: "refresh deferred: system sleep imminent".to_string(),
        }
    }
}

/// Rule, end to end: a credential-less 401 with transiently-failing recovery parks and the
/// turn completes once a refresh lands. Deliberate exclusions (budgeted children,
/// compact-path 401s) stay terminal.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn deferred_recovery_credential_less_401_parks_and_survives() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                FRESH_TOKEN,
            )
            .await
            .expect("mock inference server");

            let refresher = Arc::new(DeferredRefreshNeverLands::default());
            let server_rejected_calls = refresher.server_rejected_calls.clone();
            let pre_request_calls = refresher.pre_request_calls.clone();
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, updates) = session_token_actor(&server, am, ActorShape::default()).await;

            // Out-of-band driver: 30 virtual seconds in, a token lands and the notify fires.
            let waker = actor.auth_manager.clone().expect("actor has auth manager");
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                waker.hot_swap(GrokAuth {
                    key: FRESH_TOKEN.to_string(),
                    auth_mode: AuthMode::Oidc,
                    refresh_token: Some("rt-new".into()),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    ..GrokAuth::test_default()
                });
                waker.refresh_notifier().notify_waiters();
            });

            let outcome = run_prompt_with_cap(&actor, "deferred-recovery-parks", 86_400).await;
            assert!(
                outcome.is_ok(),
                "credential-less 401 with deferred recovery must park and survive: {outcome:?}"
            );

            let inference: Vec<_> = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .collect();
            // Send 1: credential-less, 401, one recovery dispatch fails → park.
            // Middle sends: parked resubmits, still credential-less, paced by the escalating schedule, no further dispatch.
            // Final send: authenticated once the out-of-band token lands.
            assert!(
                inference.len() >= 3,
                "expected credential-less send, parked resubmit(s), and an \
                 authenticated resubmit; got {}",
                inference.len()
            );
            let Some((_, prefix)) = inference.split_last() else {
                panic!("expected inference requests: {inference:?}");
            };
            for (i, req) in prefix.iter().enumerate() {
                assert_eq!(
                    req.authorization, None,
                    "send {i} precedes the token landing and must carry no \
                     Authorization header"
                );
            }
            assert_eq!(
                inference.last().unwrap().authorization.as_deref(),
                Some(&format!("Bearer {FRESH_TOKEN}") as &str),
                "final resubmit must carry the freshly landed bearer"
            );
            assert_eq!(
                server_rejected_calls.load(Ordering::SeqCst),
                2,
                "exactly one recovery dispatch (two attempts) — parked cycles \
                 must not dispatch"
            );
            // Exact by design: every preflight belongs to the pre-park iteration; a
            // regression letting parked cycles refresh shows up as extra dispatches.
            assert_eq!(
                pre_request_calls.load(Ordering::SeqCst),
                4,
                "parked cycles must not drive preflight refreshes"
            );
            assert!(
                terminal_failure(&updates).is_none(),
                "a surviving turn must not report a terminal retryState"
            );
            // Uncharged iterations must report the uncharged budget, not the charged one.
            assert_retrying(
                &updates,
                inference.len() - 1,
                AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS,
                &["Re-authenticating after 401", "carried no credential"],
            );
        })
        .await;
}

/// Rule: budgeted workflow children are excluded from the park — the output-budget gate
/// fails the turn closed before the auth arms run.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn budgeted_workflow_child_credential_less_401_stays_terminal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                "never-issued-token",
            )
            .await
            .expect("mock inference server");

            let refresher = Arc::new(DeferredRefreshNeverLands::default());
            let server_rejected_calls = refresher.server_rejected_calls.clone();
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, _updates) = session_token_actor(
                &server,
                am,
                ActorShape {
                    task_output_budget: Some(100_000),
                    ..Default::default()
                },
            )
            .await;

            let outcome = run_prompt_with_cap(&actor, "budgeted-child-terminal", 86_400).await;
            let err = outcome.expect_err("a budgeted child's credential-less 401 must be terminal");
            let rendered = serde_json::to_string(&err.data).unwrap_or_default();
            assert!(
                rendered.contains("budgeted workflow child model request failed"),
                "failure must classify via the output-budget gate, got: {rendered}"
            );

            let inference: Vec<_> = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .collect();
            assert_eq!(
                inference.len(),
                1,
                "budgeted children fail on the first 401: no parked resubmits"
            );
            assert_eq!(
                server_rejected_calls.load(Ordering::SeqCst),
                0,
                "the output-budget gate runs before any recovery dispatch"
            );
        })
        .await;
}

/// Rule: a transient Api 5xx mid-park proves nothing about the credential — the next
/// credential-less 401 re-parks without a fresh recovery dispatch.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn api_5xx_during_park_does_not_unpark_or_redispatch() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                FRESH_TOKEN,
            )
            .await
            .expect("mock inference server");
            // Scripts outrank the auth gate, so rungs are positional: send 1 parks (401), the first parked resubmit burns an edge-502 streak (every sampler-internal retry must 5xx for the.
            // Api error to surface), and later sends fall back to the gate.
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::json(401, serde_json::json!({ "error": "missing API key" })),
            );
            for _ in 0..4 {
                server.enqueue_response(
                    "/v1/responses",
                    ScriptedResponse::json(502, serde_json::json!({ "error": "bad gateway" })),
                );
            }

            let refresher = Arc::new(DeferredRefreshNeverLands::default());
            let server_rejected_calls = refresher.server_rejected_calls.clone();
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, updates) = session_token_actor(&server, am, ActorShape::default()).await;

            let waker = actor.auth_manager.clone().expect("actor has auth manager");
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                waker.hot_swap(GrokAuth {
                    key: FRESH_TOKEN.to_string(),
                    auth_mode: AuthMode::Oidc,
                    refresh_token: Some("rt-new".into()),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    ..GrokAuth::test_default()
                });
                waker.refresh_notifier().notify_waiters();
            });

            let outcome = run_prompt_with_cap(&actor, "park-survives-5xx", 86_400).await;
            assert!(
                outcome.is_ok(),
                "the park must ride out an interleaved 5xx: {outcome:?}"
            );
            assert_eq!(
                server_rejected_calls.load(Ordering::SeqCst),
                2,
                "the 5xx must not un-park the turn: only the pre-park recovery \
                 dispatch (two attempts) may run"
            );
            assert!(
                terminal_failure(&updates).is_none(),
                "a surviving turn must not report a terminal retryState"
            );
        })
        .await;
}

/// Rule: a parked resubmit's 429 waits never re-prepare — the mid-wait
/// re-prepare is park-gated, else each wait drives one refresh through the
/// shared budget. Subagent-shaped: only subagent turns get a wait budget.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn parked_429_wait_does_not_drive_refreshes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            // One 429 rung, then the default 200: exactly one paced wait.
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::json(429, serde_json::json!({ "error": "rate limited" })),
            );

            let refresher = Arc::new(DeferredRefreshNeverLands::default());
            let pre_request_calls = refresher.pre_request_calls.clone();
            let server_rejected_calls = refresher.server_rejected_calls.clone();
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, _updates) = session_token_actor(
                &server,
                am,
                ActorShape {
                    is_subagent: true,
                    ..Default::default()
                },
            )
            .await;

            let request = super::rate_limit_backoff_tests::conversation_request(&actor).await;
            let mut budget = actor.rate_limit_wait_budget(None);
            let outcome = tokio::time::timeout(
                Duration::from_secs(300),
                actor.run_turn_via_sampler(
                    request,
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Parked,
                ),
            )
            .await
            .expect("turn must finish within timeout");
            assert!(
                matches!(outcome, Ok(SamplerTurnOutcome::Response(..))),
                "the parked resubmit must recover after the paced wait"
            );
            assert_eq!(budget.attempts_used(), 1, "the pacer must own the 429 wait");
            // The expired token makes every prepare observable: a re-prepare
            // during the wait would dispatch at least one preflight refresh.
            assert_eq!(
                pre_request_calls.load(Ordering::SeqCst),
                0,
                "a parked iteration's 429 waits must not drive preflight refreshes"
            );
            assert_eq!(
                server_rejected_calls.load(Ordering::SeqCst),
                0,
                "no recovery dispatch may run inside the rate-limit wait loop"
            );
        })
        .await;
}

/// Rule: two-pass prefire must not re-arm while parked — pass-1 drives auth, and each
/// spawn costs an extra credential-less send the runaway guard never counts.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn parked_turn_does_not_respawn_two_pass_prefire() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                FRESH_TOKEN,
            )
            .await
            .expect("mock inference server");

            let refresher = Arc::new(DeferredRefreshNeverLands::default());
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, _updates) = session_token_actor(&server, am, ActorShape::default()).await;

            // Two-pass on, usage in the prefire band: 77% of 100k vs the 85%
            // threshold (the band opens at threshold − 10).
            {
                let mut agent_slot = actor.agent.borrow_mut();
                let agent = &*agent_slot;
                let mut policy = agent.compaction_policy().clone();
                policy.two_pass_enabled = true;
                *agent_slot = distill_agent::Agent::new(
                    agent.definition().clone(),
                    agent.prompt_context().clone(),
                    agent.system_prompt().to_string(),
                    std::sync::Arc::clone(agent.tool_bridge()),
                    agent.reminder_policy().clone(),
                    policy,
                    vec![],
                    false,
                );
            }
            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            cfg.context_window = std::num::NonZeroU64::new(100_000).expect("non-zero");
            actor.chat_state_handle.update_sampling_config(cfg);
            // ≥4 items so pass-1 survives its too-small check and reaches the auth-driving stage.
            use distill_sampling_types::ConversationItem;
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("q1"),
                ConversationItem::assistant("a1"),
                ConversationItem::user("q2"),
                ConversationItem::assistant("a2"),
            ]);
            actor.chat_state_handle.record_token_usage(77_000);

            let waker = actor.auth_manager.clone().expect("actor has auth manager");
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                waker.hot_swap(GrokAuth {
                    key: FRESH_TOKEN.to_string(),
                    auth_mode: AuthMode::Oidc,
                    refresh_token: Some("rt-new".into()),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    ..GrokAuth::test_default()
                });
                waker.refresh_notifier().notify_waiters();
            });

            let outcome = run_prompt_with_cap(&actor, "parked-prefire", 86_400).await;
            assert!(outcome.is_ok(), "parked turn must survive: {outcome:?}");

            // Pass-1 sends: /responses requests without the foreground turn header.
            let pass1_sends = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .filter(|r| r.header("x-grok-turn-idx").is_none())
                .count();
            assert_eq!(
                pass1_sends, 1,
                "parked cycles must not re-spawn prefire pass-1"
            );
        })
        .await;
}

/// Rule: parked turns must not fire the pre-sampling auto-compact — the
/// credential-less compact 401s into `surface_compact_auth_failure` and aborts
/// the very turn the park is keeping alive.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn parked_turn_past_compact_threshold_does_not_auto_compact() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                FRESH_TOKEN,
            )
            .await
            .expect("mock inference server");

            let refresher = Arc::new(DeferredRefreshNeverLands::default());
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, updates) = session_token_actor(&server, am, ActorShape::default()).await;

            // 100k window, 85% threshold (create_test_actor): the seeded 90k
            // usage puts every check past the auto-compact trigger.
            let mut cfg = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            cfg.context_window = std::num::NonZeroU64::new(100_000).expect("non-zero");
            actor.chat_state_handle.update_sampling_config(cfg);
            // Enough items that a leaked compact would really sample instead of
            // short-circuiting on a too-small conversation.
            use distill_sampling_types::ConversationItem;
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("you are a coding agent"),
                ConversationItem::user("q1"),
                ConversationItem::assistant("a1"),
                ConversationItem::user("q2"),
                ConversationItem::assistant("a2"),
            ]);

            // Usage crosses the threshold only after the first iteration's compact check (which runs un-parked at t=0): the first parked resubmit is paced ≥1s out, so a 500ms seed lands between the park and every parked.
            let usage_seeder = actor.chat_state_handle.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                usage_seeder.record_token_usage(90_000);
            });

            // Out-of-band recovery, 30 virtual seconds in.
            let waker = actor.auth_manager.clone().expect("actor has auth manager");
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                waker.hot_swap(GrokAuth {
                    key: FRESH_TOKEN.to_string(),
                    auth_mode: AuthMode::Oidc,
                    refresh_token: Some("rt-new".into()),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    ..GrokAuth::test_default()
                });
                waker.refresh_notifier().notify_waiters();
            });

            let outcome = run_prompt_with_cap(&actor, "parked-no-auto-compact", 86_400).await;
            assert!(
                outcome.is_ok(),
                "a parked turn past the compact threshold must stay parked and \
                 survive: {outcome:?}"
            );
            assert!(
                terminal_failure(&updates).is_none(),
                "a surviving turn must not report a terminal retryState"
            );
            // Compact sends are the only /responses requests without the
            // foreground turn header (two-pass prefire stays off here).
            let compact_sends = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .filter(|r| r.header("x-grok-turn-idx").is_none())
                .count();
            assert_eq!(
                compact_sends, 0,
                "parked iterations must not send a credential-less compact"
            );
        })
        .await;
}

/// Fails transiently until `recovers` flips, then mints [`FRESH_TOKEN`]; counts
/// `ServerRejected` dispatches throughout.
#[derive(Default)]
struct DeferredThenRecovers {
    server_rejected_calls: Arc<AtomicU32>,
    recovers: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl distill_login::refresh::TokenRefresher for DeferredThenRecovers {
    async fn refresh(
        &self,
        reason: distill_login::refresh::RefreshReason,
    ) -> distill_login::refresh::RefreshOutcome {
        if reason == distill_login::refresh::RefreshReason::ServerRejected {
            self.server_rejected_calls.fetch_add(1, Ordering::SeqCst);
        }
        if !self.recovers.load(Ordering::SeqCst) {
            return distill_login::refresh::RefreshOutcome::TransientFailure {
                message: "refresh deferred: system sleep imminent".to_string(),
            };
        }
        distill_login::refresh::RefreshOutcome::success(GrokAuth {
            key: FRESH_TOKEN.to_string(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        })
    }
}

/// Rule: a `Sent` rejection leaves the park path onto the charged budget (terminating
/// after `MAX_RETRIES`) — pins the `is_missing()` conjunct of the re-park arm.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn parked_turn_authenticated_rejection_dispatches_and_exhausts_charged() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // The server accepts a token nobody mints: 401s before and after the landing.
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                "never-issued-token",
            )
            .await
            .expect("mock inference server");

            let refresher = Arc::new(DeferredThenRecovers::default());
            let server_rejected_calls = refresher.server_rejected_calls.clone();
            let recovers = refresher.recovers.clone();
            let (_dir, am) = expired_auth_manager(refresher);
            let (actor, updates) = session_token_actor(&server, am, ActorShape::default()).await;

            // 30 virtual seconds in, a wire-valid token lands — the server keeps rejecting.
            let waker = actor.auth_manager.clone().expect("actor has auth manager");
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                recovers.store(true, Ordering::SeqCst);
                waker.hot_swap(GrokAuth {
                    key: FRESH_TOKEN.to_string(),
                    auth_mode: AuthMode::Oidc,
                    refresh_token: Some("rt-new".into()),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    ..GrokAuth::test_default()
                });
                waker.refresh_notifier().notify_waiters();
            });

            let outcome = run_prompt_with_cap(&actor, "parked-then-sent-401", 86_400).await;
            let err =
                outcome.expect_err("authenticated rejections must exhaust the charged budget");
            let rendered = serde_json::to_string(&err.data).unwrap_or_default();
            assert!(
                rendered.contains("authenticated inference requests were still rejected"),
                "exhaustion must name authenticated rejections, got: {rendered}"
            );

            let authenticated = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .filter(|r| r.authorization.as_deref() == Some(&format!("Bearer {FRESH_TOKEN}")))
                .count();
            assert_eq!(
                authenticated,
                (AuthRetrySchedule::MAX_RETRIES + 1) as usize,
                "first authenticated resubmit plus MAX_RETRIES charged retries"
            );
            // Sent rejections dispatch recovery, but it short-circuits on the wire-valid
            // landed token — only the pre-park dispatch reaches the refresher.
            assert_eq!(
                server_rejected_calls.load(Ordering::SeqCst),
                2,
                "only the pre-park recovery dispatch may reach the refresher"
            );
            // The budget flips at the landing: exactly MAX_RETRIES charged backoffs after,
            // all uncharged before — a dropped `is_missing()` conjunct breaks both halves.
            let retrying = retrying_updates(&updates);
            let (parked, charged) =
                retrying.split_at(retrying.len() - AuthRetrySchedule::MAX_RETRIES as usize);
            assert!(
                !parked.is_empty()
                    && parked
                        .iter()
                        .all(|(_, max, _)| *max == AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS),
                "pre-landing rejections ride the uncharged budget: {retrying:?}"
            );
            for (i, (attempt, max, _)) in charged.iter().enumerate() {
                assert_eq!(*attempt, (i + 1) as u32, "charged attempts renumber from 1");
                assert_eq!(
                    *max,
                    AuthRetrySchedule::MAX_RETRIES,
                    "post-landing rejections ride the charged budget: {retrying:?}"
                );
            }

            let (error_type, message) = terminal_failure(&updates)
                .expect("an exhausted turn must report a terminal retryState");
            assert_eq!(error_type, "auth", "{message}");
        })
        .await;
}

/// Rule: with a refresh that never lands, the runaway guard fails the turn with a
/// client-visible terminal retryState — not an infinite loop.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn never_recovering_credential_less_401_hits_runaway_guard() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_required_auth(
                vec![MockModelEntry::new("test")],
                "never-issued-token",
            )
            .await
            .expect("mock inference server");

            let (_dir, am) = expired_auth_manager(Arc::new(DeferredRefreshNeverLands::default()));
            let (actor, updates) = session_token_actor(&server, am, ActorShape::default()).await;

            let outcome = run_prompt_with_cap(&actor, "runaway-guard-bounded", 86_400).await;

            let err = outcome.expect_err("a never-landing refresh must fail the turn bounded");
            let rendered = serde_json::to_string(&err.data).unwrap_or_default();
            assert!(
                rendered.contains("runaway guard"),
                "failure must name the runaway guard, got: {rendered}"
            );

            let unauthenticated = server
                .requests()
                .into_iter()
                .filter(|r| r.path.contains("/responses"))
                .filter(|r| r.authorization.is_none())
                .count();
            assert_eq!(
                unauthenticated,
                (AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS + 1) as usize,
                "initial send plus MAX_UNCHARGED_RESUBMITS parked resubmits, all \
                 credential-less"
            );

            let (error_type, message) =
                terminal_failure(&updates).expect("bounded failure must reach the client");
            assert_eq!(
                error_type, "auth",
                "retries exhausted escalates SelfHealing to ManualLogin: {message}"
            );
            assert!(
                message.contains("runaway guard"),
                "the notification must carry the runaway-guard story: {message}"
            );
            // A charged `max_retries: 3` leaking in here renders "attempt 27/3" in the pager.
            assert_retrying(
                &updates,
                AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS as usize,
                AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS,
                &["Re-authenticating after 401", "carried no credential"],
            );
        })
        .await;
}
