// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::*;
use super::*;
use distill_login::{AuthManager, AuthMode, GrokAuth, GrokComConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

/// Test refresher that returns a fresh token and records that it was invoked.
/// Used to drive the auth-arm success path.
struct AlwaysSucceedRefresher {
    called: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl distill_login::refresh::TokenRefresher for AlwaysSucceedRefresher {
    async fn refresh(
        &self,
        _reason: distill_login::refresh::RefreshReason,
    ) -> distill_login::refresh::RefreshOutcome {
        self.called.store(true, Ordering::SeqCst);
        distill_login::refresh::RefreshOutcome::Success(Box::new(GrokAuth {
            key: "refreshed-test-token".to_string(),
            auth_mode: AuthMode::Oidc,
            refresh_token: Some("rt-new".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            ..GrokAuth::test_default()
        }))
    }
}

/// Test refresher that always fails transiently — the shape of a sleep-gate
/// deferral ("refresh deferred: system sleep imminent") or a network blip.
struct AlwaysTransientFailRefresher {
    called: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl distill_login::refresh::TokenRefresher for AlwaysTransientFailRefresher {
    async fn refresh(
        &self,
        _reason: distill_login::refresh::RefreshReason,
    ) -> distill_login::refresh::RefreshOutcome {
        self.called.store(true, Ordering::SeqCst);
        distill_login::refresh::RefreshOutcome::TransientFailure {
            message: "refresh deferred: system sleep imminent".to_string(),
        }
    }
}

/// `(tempdir, manager)` with an expired OIDC token loaded so `unauthorized_recovery()` actually dispatches to the refresher.
/// Tempdir must outlive the manager (auth.json path).
fn auth_manager_with_refresher(
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

/// Build a `SamplingErrorInfo` of kind Auth, the same shape the inner `OaiCompatClient` reports after recording its own attribution.
fn auth_error() -> distill_sampler::SamplingErrorInfo {
    distill_sampler::SamplingErrorInfo {
        kind: distill_sampler::SamplingErrorKind::Auth,
        message: "Unauthorized (401)".to_string(),
        status_code: Some(401),
        is_retryable: false,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
        model_metadata: None,
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: distill_sampling_types::SentCredential::Unknown,
    }
}

/// [`auth_error`] with the wire-credential provenance pinned.
fn auth_error_with_credential(
    credential: distill_sampling_types::SentCredential,
) -> distill_sampler::SamplingErrorInfo {
    distill_sampler::SamplingErrorInfo {
        credential,
        ..auth_error()
    }
}

/// Construct a test actor with the supplied `auth_manager` and session-token credentials wired in.
/// Wraps the actor in `Arc` ready for `handle_sampling_failure`.
async fn make_actor_with_auth_manager(
    auth_manager: Option<Arc<AuthManager>>,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let (actor, rx) = make_actor_parts_with_auth_manager(auth_manager).await;
    (Arc::new(actor), rx)
}

/// [`make_actor_with_auth_manager`] before the `Arc` wrap, for tests that
/// pin actor fields (e.g. the park kill switch).
async fn make_actor_parts_with_auth_manager(
    auth_manager: Option<Arc<AuthManager>>,
) -> (SessionActor, mpsc::UnboundedReceiver<PersistenceMsg>) {
    make_actor_parts_with_method_and_credentials(
        auth_manager,
        "cached_token",
        distill_chat_state::AuthType::SessionToken,
        "initial-test-key".to_string(),
    )
    .await
}

/// Variant that pins the credential `auth_type`; the `auth_method_id` is derived from it.
/// Use [`make_actor_with_method_and_credentials`] to pin the two independently.
async fn make_actor_with_auth_and_credentials(
    auth_manager: Option<Arc<AuthManager>>,
    auth_type: distill_chat_state::AuthType,
    api_key: String,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let method_id = match auth_type {
        distill_chat_state::AuthType::SessionToken => "cached_token",
        distill_chat_state::AuthType::ApiKey => "xai.api_key",
    };
    make_actor_with_method_and_credentials(auth_manager, method_id, auth_type, api_key).await
}

/// Pin the ACP `auth_method_id` and credential `auth_type` independently.
/// The gate keys off the stable `auth_method_id`, so this reproduces the regression.
/// In the regression, a session-token cache miss with `XAI_API_KEY` set transiently collapsed a session method's `creds.auth_type` to `ApiKey`.
async fn make_actor_with_method_and_credentials(
    auth_manager: Option<Arc<AuthManager>>,
    auth_method_id: &str,
    auth_type: distill_chat_state::AuthType,
    api_key: String,
) -> (Arc<SessionActor>, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let (actor, rx) = make_actor_parts_with_method_and_credentials(
        auth_manager,
        auth_method_id,
        auth_type,
        api_key,
    )
    .await;
    (Arc::new(actor), rx)
}

/// [`make_actor_with_method_and_credentials`] before the `Arc` wrap.
async fn make_actor_parts_with_method_and_credentials(
    auth_manager: Option<Arc<AuthManager>>,
    auth_method_id: &str,
    auth_type: distill_chat_state::AuthType,
    api_key: String,
) -> (SessionActor, mpsc::UnboundedReceiver<PersistenceMsg>) {
    let (gateway_tx, _) = mpsc::unbounded_channel();
    let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
    let mut actor = create_test_actor(50_000, 100_000, 85, gateway_tx, persistence_tx).await;
    actor.auth_manager = auth_manager;
    actor.auth_method_id = test_auth_method_id(auth_method_id);
    actor
        .chat_state_handle
        .update_credentials(distill_chat_state::Credentials {
            api_key: Some(api_key),
            auth_type,
            ..Default::default()
        });
    (actor, persistence_rx)
}

/// `(tempdir, manager)` holding a valid OIDC token (so `get_valid_token()` is a cache hit).
/// The tempdir must outlive the manager (auth.json path).
fn auth_manager_with_valid_token(key: &str) -> (tempfile::TempDir, Arc<AuthManager>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
    am.hot_swap(GrokAuth {
        key: key.into(),
        auth_mode: AuthMode::Oidc,
        refresh_token: Some("rt".into()),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        ..GrokAuth::test_default()
    });
    (dir, am)
}

/// Sub-case 1: with no auth_manager the handler falls through and emits no attribution.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn no_emit_when_auth_manager_is_none() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_auth_manager(None).await;
            distill_login::attribution::reset_test_emit_count();
            let _ = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert_eq!(
                distill_login::attribution::test_emit_count(),
                0,
                "auth arm must not emit attribution when no auth_manager is wired"
            );
        })
        .await;
}

/// Sub-case 2: with no AuthManager, auth recovery is skipped entirely and the 401 falls through to a terminal error.
/// Covers BYOK / API-key users where no OIDC refresh is possible.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn no_recovery_without_auth_manager() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                distill_chat_state::AuthType::ApiKey,
                "xai-byok-key".to_string(),
            )
            .await;
            distill_login::attribution::reset_test_emit_count();
            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                result.is_err(),
                "no auth manager must fall through to terminal error"
            );
            assert_eq!(
                distill_login::attribution::test_emit_count(),
                0,
                "auth arm must not emit attribution without auth manager"
            );
        })
        .await;
}

/// Session-based auth with a working refresher returns RefreshAuthAndResubmit.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_recovery_returns_refresh_and_retry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        store: RecoveredStore::SessionToken,
                        ..
                    })
                ),
                "session-based auth with a working refresher must return RefreshAuthAndResubmit"
            );
            assert!(called.load(Ordering::SeqCst), "refresher must be invoked");
        })
        .await;
}

/// Rule: a credential-less 401 with transiently-failed recovery parks on the
/// uncharged path instead of failing the turn.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn credential_less_401_with_deferred_refresh_parks_on_uncharged_resubmit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysTransientFailRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    auth_error_with_credential(distill_sampling_types::SentCredential::Missing),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        credential: distill_sampling_types::SentCredential::Missing,
                        store: RecoveredStore::SessionToken,
                    })
                ),
                "credential-less 401 with self-healing deferred refresh must park on \
                 the uncharged resubmit path"
            );
            assert!(called.load(Ordering::SeqCst), "refresher must be consulted");
        })
        .await;
}

/// Rule: an already-parked credential-less 401 re-parks without touching the
/// refresher — parked cycles must not consume the shared escalation budget.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn parked_credential_less_401_reparks_without_recovery_dispatch() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysTransientFailRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    auth_error_with_credential(distill_sampling_types::SentCredential::Missing),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Parked,
                )
                .await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        credential: distill_sampling_types::SentCredential::Missing,
                        store: RecoveredStore::SessionToken,
                    })
                ),
                "an already-parked credential-less 401 must re-park"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "a parked cycle must not dispatch a recovery refresh"
            );
        })
        .await;
}

/// Rule: a credential-less 401 on a Length-salvage continuation still parks, fresh or
/// parked — the quiet truncated-complete arm excludes `Auth` kinds, so it neither
/// completes the turn truncated nor goes terminal.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn mid_salvage_credential_less_401_still_parks() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for park in [TurnParkState::Fresh, TurnParkState::Parked] {
                let called = Arc::new(AtomicBool::new(false));
                let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                    Arc::new(AlwaysTransientFailRefresher {
                        called: called.clone(),
                    });
                let (_dir, am) = auth_manager_with_refresher(refresher);
                let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
                // A window the seeded estimate exceeds: only the quiet arm's
                // `Auth` exclusion keeps this 401 out of it.
                let error = distill_sampler::SamplingErrorInfo {
                    model_metadata: Some(distill_sampling_types::ResponseModelMetadata {
                        context_window: Some(1),
                        max_completion_tokens: None,
                        models_etag: None,
                    }),
                    ..auth_error_with_credential(distill_sampling_types::SentCredential::Missing)
                };
                let result = actor
                    .handle_sampling_failure(error, 0, transient_state(0, true), true, park)
                    .await;
                assert!(
                    matches!(
                        result,
                        Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                            credential: distill_sampling_types::SentCredential::Missing,
                            store: RecoveredStore::SessionToken,
                        })
                    ),
                    "a mid-salvage credential-less 401 must park ({park:?})"
                );
                assert_eq!(
                    called.load(Ordering::SeqCst),
                    !park.is_parked(),
                    "one recovery dispatch when fresh, none when already parked ({park:?})"
                );
            }
        })
        .await;
}

/// Rule: a credentialed (or `Unknown` — fails closed) 401 stays terminal when recovery fails.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn credentialed_401_with_deferred_refresh_stays_terminal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for credential in [
                distill_sampling_types::SentCredential::Sent,
                distill_sampling_types::SentCredential::Unknown,
            ] {
                let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                    Arc::new(AlwaysTransientFailRefresher {
                        called: Arc::new(AtomicBool::new(false)),
                    });
                let (_dir, am) = auth_manager_with_refresher(refresher);
                let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
                let result = actor
                    .handle_sampling_failure(
                        auth_error_with_credential(credential),
                        0,
                        transient_state(0, true),
                        false,
                        TurnParkState::Fresh,
                    )
                    .await;
                assert!(
                    result.is_err(),
                    "{credential:?} 401 with failed recovery must stay terminal"
                );
            }
        })
        .await;
}

/// Rule: kill switch off restores terminal behavior for the exact input that otherwise parks.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn credential_less_401_with_park_kill_switch_stays_terminal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysTransientFailRefresher {
                    called: Arc::new(AtomicBool::new(false)),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (mut actor, _rx) = make_actor_parts_with_auth_manager(Some(am)).await;
            actor.uncharged_401_park_enabled = false;
            let actor = Arc::new(actor);
            let result = actor
                .handle_sampling_failure(
                    auth_error_with_credential(distill_sampling_types::SentCredential::Missing),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                result.is_err(),
                "kill switch off: the otherwise-parking input must stay terminal"
            );
        })
        .await;
}

/// Rule: a credential-less 401 stays terminal under a provider refresh authority —
/// parking would loop on a token only an interactive flow can mint.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn credential_less_401_with_provider_authority_stays_terminal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(
                dir.path(),
                GrokComConfig {
                    auth_provider_command: Some("acme-auth".to_owned()),
                    auth_provider_label: Some("Acme SSO".to_owned()),
                    ..GrokComConfig::default()
                },
            ));
            am.hot_swap(GrokAuth {
                key: "expired-external".into(),
                auth_mode: AuthMode::External,
                expires_at: Some(chrono::Utc::now() - chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            });
            am.set_refresher(Arc::new(AlwaysTransientFailRefresher {
                called: Arc::new(AtomicBool::new(false)),
            }));
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    auth_error_with_credential(distill_sampling_types::SentCredential::Missing),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                result.is_err(),
                "credential-less 401 under a provider refresh authority must stay terminal"
            );
        })
        .await;
}

/// Rule: a credential-less 401 stays terminal when the remedy is a manual re-login —
/// resubmits would loop on a credential no refresh can mint.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn credential_less_401_with_permanent_failure_stays_terminal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysTransientFailRefresher {
                    called: Arc::new(AtomicBool::new(false)),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            am.record_permanent_failure(
                "initial-test-key".to_string(),
                distill_login::error::RefreshTokenFailedReason::RefreshTokenRejected.into(),
            );
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    auth_error_with_credential(distill_sampling_types::SentCredential::Missing),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                result.is_err(),
                "credential-less 401 with a permanent refresh failure must stay terminal"
            );
        })
        .await;
}

/// Regression: sampler 401 with API-key auth (BYOK `env_key` / `XAI_API_KEY`) must NOT attempt an OIDC session-token refresh.
/// The bearer on the wire is the static API key, so refreshing the session token reports success but the retry re-sends the same rejected key.
/// Recovery is skipped and the 401 surfaces as a terminal error.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn sampler_401_with_api_key_auth_skips_refresh_and_surfaces_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am),
                distill_chat_state::AuthType::ApiKey,
                "xai-byok-key".to_string(),
            )
            .await;

            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;

            assert!(
                result.is_err(),
                "API-key 401 must surface a terminal error, not retry"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "API-key 401 must NOT trigger an OIDC session-token refresh"
            );
        })
        .await;
}

/// Per-turn pre-flight refresh must not fire when `creds.auth_type` is `ApiKey` (a BYOK model).
/// The model's own API key must not be overwritten by the session JWT.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refresh_skips_api_key_auth_type() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am),
                distill_chat_state::AuthType::ApiKey,
                "byok-api-key".to_string(),
            )
            .await;
            actor.refresh_token_if_expired().await;
            assert!(
                !called.load(Ordering::SeqCst),
                "pre-flight refresh must NOT fire for ApiKey auth_type"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("byok-api-key"),
                "BYOK api_key must not be overwritten by session token refresh"
            );
        })
        .await;
}

/// Hard-expired session token: pre-flight must call the refresher and must not leave credentials stuck while pretending the JWT/config path applies.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refreshes_hard_expired_session_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            assert!(
                !am.has_usable_token(),
                "precondition: access token is hard-expired"
            );

            let (actor, _rx) = make_actor_with_auth_manager(Some(am.clone())).await;
            actor.refresh_token_if_expired().await;

            assert!(
                called.load(Ordering::SeqCst),
                "pre-flight must invoke the refresher for a hard-expired session token"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("refreshed-test-token"),
                "credentials must be updated to the refreshed bearer"
            );
            assert!(am.has_usable_token());
        })
        .await;
}

/// Hard-expired token and a failed refresh: do not fall through to JWT/config.toml.
/// Strip the chat-state seed so default headers cannot carry a dead access token.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_hard_expired_refresh_failure_skips_jwt_fallthrough() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> = Arc::new({
                struct AlwaysFail(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl distill_login::refresh::TokenRefresher for AlwaysFail {
                    async fn refresh(
                        &self,
                        _: distill_login::refresh::RefreshReason,
                    ) -> distill_login::refresh::RefreshOutcome {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        distill_login::refresh::RefreshOutcome::transient("refresh failed")
                    }
                }
                AlwaysFail(call_count.clone())
            });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_manager(Some(am.clone())).await;

            actor.refresh_token_if_expired().await;

            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "pre-flight must attempt refresh"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                None,
                "hard-expired pre-flight failure must strip the chat-state seed"
            );
            assert!(
                !am.has_usable_token(),
                "token remains hard-expired after failed refresh"
            );
            assert!(
                am.permanent_failure().is_none(),
                "transient refresh failure must not poison permanent_failure"
            );
        })
        .await;
}

/// Soft-expired (inside the early-invalidation buffer) and a transient refresh failure: retain the seed.
/// The access token on the wire is still accepted, so it can continue until 401 recovery.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_soft_expired_transient_fail_retains_seed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> = Arc::new({
                struct AlwaysFail(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl distill_login::refresh::TokenRefresher for AlwaysFail {
                    async fn refresh(
                        &self,
                        _: distill_login::refresh::RefreshReason,
                    ) -> distill_login::refresh::RefreshOutcome {
                        self.0.fetch_add(1, Ordering::SeqCst);
                        distill_login::refresh::RefreshOutcome::transient("refresh failed")
                    }
                }
                AlwaysFail(call_count.clone())
            });
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            // Inside the early-invalidation buffer but still hard-valid.
            am.hot_swap(GrokAuth {
                key: "buffered-test-key".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::seconds(30)),
                ..GrokAuth::test_default()
            });
            am.set_refresher(refresher);
            let (actor, _rx) = make_actor_with_auth_and_credentials(
                Some(am.clone()),
                distill_chat_state::AuthType::SessionToken,
                "buffered-test-key".to_string(),
            )
            .await;

            actor.refresh_token_if_expired().await;

            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "soft-expired pre-flight must still attempt refresh"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("buffered-test-key"),
                "buffer-window soft-expired + transient fail must retain seed"
            );
            assert!(
                am.has_usable_token(),
                "token inside hard-expiry buffer remains usable"
            );
        })
        .await;
}

/// Proactive refresh keeps the cache hot so `refresh_token_if_expired` (per-turn pre-flight) is a cache hit.
/// The refresher fires once (proactive), then the per-turn call sees the fresh token without hitting the identity provider again.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn proactive_refresh_makes_per_turn_refresh_a_cache_hit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> = Arc::new({
                struct Counting(Arc<std::sync::atomic::AtomicU32>);
                #[async_trait::async_trait]
                impl distill_login::refresh::TokenRefresher for Counting {
                    async fn refresh(
                        &self,
                        _: distill_login::refresh::RefreshReason,
                    ) -> distill_login::refresh::RefreshOutcome {
                        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        distill_login::refresh::RefreshOutcome::Success(Box::new(GrokAuth {
                            key: "proactive-fresh".into(),
                            auth_mode: AuthMode::Oidc,
                            refresh_token: Some("rt-new".into()),
                            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                            ..GrokAuth::test_default()
                        }))
                    }
                }
                Counting(call_count.clone())
            });

            let (_dir, am) = auth_manager_with_refresher(refresher);
            let cancel = tokio_util::sync::CancellationToken::new();
            am.start_proactive_refresh(cancel.clone());

            // Wait for the proactive task to fire; its first pass runs after PROACTIVE_MIN_SLEEP, so the window must exceed the floor
            tokio::time::sleep(
                distill_login::manager::PROACTIVE_MIN_SLEEP
                    + std::time::Duration::from_millis(1000),
            )
            .await;
            assert!(
                call_count.load(Ordering::SeqCst) >= 1,
                "proactive task must have fired"
            );
            let count_after_proactive = call_count.load(Ordering::SeqCst);

            // Now run refresh_token_if_expired (the per-turn pre-flight).
            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            actor.refresh_token_if_expired().await;

            assert_eq!(
                call_count.load(Ordering::SeqCst),
                count_after_proactive,
                "per-turn refresh must NOT call the refresher again (cache hit)"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("proactive-fresh"),
                "per-turn refresh must pick up the proactively-refreshed token"
            );

            cancel.cancel();
        })
        .await;
}

fn model_not_found_error() -> distill_sampler::SamplingErrorInfo {
    distill_sampler::SamplingErrorInfo {
            kind: distill_sampler::SamplingErrorKind::Api,
            message: "API error (status 404 Not Found): The model Distill does not exist or your team does not have access".into(),
            status_code: Some(404),
            is_retryable: false,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: distill_sampling_types::SentCredential::Unknown,
        }
}

/// 404 model-not-found with a legacy WebLogin token appends a "Legacy auth detected" hint to the error message.
#[tokio::test(flavor = "current_thread")]
async fn legacy_auth_hint_on_404_model_not_found() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(GrokAuth {
                key: "legacy-token".into(),
                auth_mode: AuthMode::WebLogin,
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    model_not_found_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                msg.contains("deprecated authentication method"),
                "404 with WebLogin must include deprecation message, got: {msg}"
            );
            assert!(
                msg.contains("grok update"),
                "hint must mention `grok update` before re-login, got: {msg}"
            );
            assert!(
                msg.contains("grok logout"),
                "hint must mention `grok logout`, got: {msg}"
            );
            assert!(
                msg.contains("grok login"),
                "hint must mention `grok login`, got: {msg}"
            );
            let update_at = msg.find("grok update").expect("grok update");
            let logout_at = msg.find("grok logout").expect("grok logout");
            assert!(
                update_at < logout_at,
                "update must come before logout, got: {msg}"
            );
            assert!(
                msg.contains("Version:"),
                "must show client version, got: {msg}"
            );
        })
        .await;
}

/// In production, 401s arrive as `SamplingErrorKind::Auth` with `status_code: None`.
/// Step 4b intercepts `Auth`-kind errors and runs the full recovery chain.
/// Using `Api` kind with `status_code: Some(401)` exercises the hint condition (`status_code == Some(401)`) without triggering recovery.
fn unauthorized_401_error() -> distill_sampler::SamplingErrorInfo {
    distill_sampler::SamplingErrorInfo {
            kind: distill_sampler::SamplingErrorKind::Api,
            message: "Unauthorized (401) from https://cli-chat-proxy.grok.com/v1/responses: {\"error\":\"Invalid or expired credentials (auth_kind=bearer, x_xai_token_auth=xai-grok-cli, upstream=Unauthenticated, reason=no auth context)\"}".into(),
            status_code: Some(401),
            is_retryable: false,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: distill_sampling_types::SentCredential::Unknown,
        }
}

/// 401 Unauthorized with a legacy WebLogin token appends a "Legacy auth detected" hint to the error message.
#[tokio::test(flavor = "current_thread")]
async fn legacy_auth_hint_on_401_unauthorized() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(GrokAuth {
                key: "legacy-token".into(),
                auth_mode: AuthMode::WebLogin,
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    unauthorized_401_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data.as_str().unwrap();
            assert!(
                msg.contains("deprecated authentication method"),
                "401 with WebLogin must include deprecation message, got: {msg}"
            );
            assert!(
                msg.contains("grok update"),
                "hint must mention `grok update` before re-login, got: {msg}"
            );
            assert!(
                msg.contains("grok logout"),
                "hint must mention `grok logout`, got: {msg}"
            );
            assert!(
                msg.contains("grok login"),
                "hint must mention `grok login`, got: {msg}"
            );
            let update_at = msg.find("grok update").expect("grok update");
            let logout_at = msg.find("grok logout").expect("grok logout");
            assert!(
                update_at < logout_at,
                "update must come before logout, got: {msg}"
            );
        })
        .await;
}

/// 401 with OIDC auth must NOT append the legacy hint.
#[tokio::test(flavor = "current_thread")]
async fn no_legacy_hint_on_401_for_oidc_auth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(GrokAuth {
                key: "oidc-token".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    unauthorized_401_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| data.as_str())
                .unwrap();
            assert!(
                !msg.contains("deprecated authentication method"),
                "OIDC auth must NOT trigger WebLogin deprecation on 401, got: {msg}"
            );
            assert!(
                msg.contains("Auth:      Oidc"),
                "OIDC 401 must show auth mode in enriched message, got: {msg}"
            );
        })
        .await;
}

/// 404 model-not-found with OIDC auth must NOT append the legacy hint.
#[tokio::test(flavor = "current_thread")]
async fn no_legacy_hint_for_oidc_auth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(GrokAuth {
                key: "oidc-token".into(),
                auth_mode: AuthMode::Oidc,
                refresh_token: Some("rt".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                ..GrokAuth::test_default()
            });

            let (actor, _rx) = make_actor_with_auth_manager(Some(am)).await;
            let result = actor
                .handle_sampling_failure(
                    model_not_found_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            let err = match result {
                Err(e) => e,
                Ok(_) => panic!("expected Err from handle_sampling_failure"),
            };
            let data = err.data.unwrap();
            let msg = data
                .get("message")
                .and_then(|v| v.as_str())
                .or_else(|| data.as_str())
                .unwrap();
            assert!(
                !msg.contains("deprecated authentication method"),
                "OIDC auth must NOT trigger WebLogin deprecation, got: {msg}"
            );
            assert!(
                msg.contains("Auth:      Oidc"),
                "OIDC 404 must show auth mode in enriched message, got: {msg}"
            );
            assert!(
                msg.contains("Version:"),
                "OIDC 404 must show version in enriched message, got: {msg}"
            );
        })
        .await;
}

// Regression group: a live session whose `auth_type` transiently reads `ApiKey` must still recover
// The gate keys off the stable `auth_method_id`
#[test]
fn session_token_auth_gate_truth_table() {
    use crate::agent::auth_method::{ModelByok, session_token_auth_gate as gate};
    // Non-session methods never refresh, regardless of BYOK status or endpoint.
    for fp in [false, true] {
        assert!(!gate(false, ModelByok::NotByok, fp));
        assert!(!gate(false, ModelByok::Byok, fp));
        assert!(!gate(false, ModelByok::Unknown, fp));
        // Session method: a definite classification ignores the endpoint
        // NotByok always refreshes (it only ever routes to the session endpoint); a genuine per-model Byok never does
        assert!(gate(true, ModelByok::NotByok, fp));
        assert!(!gate(true, ModelByok::Byok, fp));
    }
    // Unknown BYOK: refresh only against a first-party Grok host.
    // That way a transiently-unclassifiable config can't demote a live session (the stale-token 401 regression).
    // The session token still never leaks to a third-party BYOK endpoint.
    assert!(gate(true, ModelByok::Unknown, true));
    assert!(!gate(true, ModelByok::Unknown, false));
}

/// Pre-fix, the gate read `auth_type` and skipped recovery here, 401'ing every turn until restart.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_session_method_with_stale_api_key_auth_type_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                distill_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;

            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "session-based method must recover even when auth_type transiently reads ApiKey"
            );
            assert!(
                called.load(Ordering::SeqCst),
                "the OIDC refresher must be invoked for a session-based method"
            );
        })
        .await;
}

/// Same regression via the `oidc` method id (the other session-based variant).
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_oidc_method_with_stale_api_key_auth_type_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "oidc",
                distill_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;

            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "oidc method must recover even when auth_type transiently reads ApiKey"
            );
            assert!(
                called.load(Ordering::SeqCst),
                "the OIDC refresher must be invoked"
            );
        })
        .await;
}

/// Without the live bearer resolver here the sampler would sign requests with the stale buffered token.
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_wires_bearer_resolver_for_session_method_despite_api_key_auth_type()
 {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                distill_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_some(),
                "session-based method must use the live bearer resolver, not the buffered key"
            );
        })
        .await;
}

/// Negative: a genuine `xai.api_key` method keeps its configured key on the wire (no live resolver).
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_no_bearer_resolver_for_api_key_method() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "xai.api_key",
                distill_chat_state::AuthType::ApiKey,
                "xai-static-key".to_string(),
            )
            .await;

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_none(),
                "api-key method must keep its configured bearer (no live resolver)"
            );
        })
        .await;
}

/// The pre-flight refresh heals a transiently-`ApiKey` session by writing the fresh session token back into `creds.api_key`.
#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(attribution_emit_count)]
async fn pre_flight_refresh_heals_session_method_with_stale_api_key_auth_type() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                distill_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            actor.refresh_token_if_expired().await;

            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("fresh-session-token"),
                "session-based pre-flight refresh must heal a stale api_key with the live token"
            );
        })
        .await;
}

/// End-to-end regression: a session born on `xai.api_key` (the gate inactive) must adopt a later OIDC `/login` on the SAME actor.
/// The shared `auth_method_id` handle is flipped in place (no re-spawn), so the next turn wires the live bearer resolver and heals the stale key.
#[tokio::test(flavor = "current_thread")]
async fn session_born_on_api_key_recovers_after_oidc_login_without_restart() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("fresh-oidc-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "xai.api_key",
                distill_chat_state::AuthType::ApiKey,
                "stale-session-jwt".to_string(),
            )
            .await;

            // Born on api_key: the gate is inactive, so no live resolver.
            assert!(
                actor
                    .reconstruct_full_config()
                    .await
                    .bearer_resolver
                    .is_none(),
                "api-key session must not use the live resolver before login"
            );

            // Simulate the agent's `authenticate` publishing an OIDC method into the shared handle this running actor already holds (no re-spawn)
            actor
                .auth_method_id
                .store(Some(std::sync::Arc::new(acp::AuthMethodId::new("oidc"))));

            // The gate is recomputed each turn from the shared handle, so the flip alone activates the live resolver on the very next turn
            // That happens with no re-spawn and before any token refresh runs
            assert!(
                actor
                    .reconstruct_full_config()
                    .await
                    .bearer_resolver
                    .is_some(),
                "flipping the shared handle activates the resolver on the next turn"
            );

            // The pre-flight refresh then heals the stale api_key with the live token.
            actor.refresh_token_if_expired().await;
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_credentials()
                    .await
                    .api_key
                    .as_deref(),
                Some("fresh-oidc-token"),
                "the stale api_key must be healed with the fresh OIDC token"
            );
        })
        .await;
}

// Per-model BYOK memo (`SessionActor::model_auth_memo`): a definite cached status is served without recomputing, and the memo keys on `model_id`

/// The cache-hit branch is what lets a later config parse failure (`Unknown`) fall back to the last-known-good status.
#[tokio::test(flavor = "current_thread")]
async fn model_auth_memo_serves_cached_status_and_keys_on_model() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                distill_chat_state::AuthType::SessionToken,
                "k".to_string(),
            )
            .await;

            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: "model-a".to_string(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::Byok,
                        auth_scheme: Default::default(),
                    },
                    provider: None,
                }));

            // Cache hit: served without consulting config.
            assert_eq!(actor.model_auth_facts("model-a").byok, ModelByok::Byok);

            // Different model re-resolves rather than serving the stale `Byok`.
            assert_ne!(actor.model_auth_facts("model-b").byok, ModelByok::Byok);
        })
        .await;
}

/// A session method whose active model is a genuine per-model BYOK model keeps the model's own key on the wire (no live resolver).
#[tokio::test(flavor = "current_thread")]
async fn reconstruct_full_config_no_bearer_resolver_for_byok_model_on_session_method() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (_dir, am) = auth_manager_with_valid_token("session-token");
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                distill_chat_state::AuthType::SessionToken,
                "byok-key".to_string(),
            )
            .await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model,
                    facts: ModelAuthFacts {
                        byok: ModelByok::Byok,
                        auth_scheme: Default::default(),
                    },
                    provider: None,
                }));

            let cfg = actor.reconstruct_full_config().await;

            assert!(
                cfg.bearer_resolver.is_none(),
                "a per-model BYOK model must keep its own key even on a session method"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_preserves_existing_conversation_group() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                distill_chat_state::AuthType::SessionToken,
                "k".to_string(),
            )
            .await;
            let expected_group: crate::sampling::ConversationGroupId =
                "111fc242-925b-5a7d-826e-2974daad239f".into();
            let mut current = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            current.conversation_group_id = Some(expected_group.clone());
            actor.chat_state_handle.update_sampling_config(current);

            let mut incoming = actor.reconstruct_full_config().await;
            incoming.conversation_group_id = None;
            actor
                .handle_set_session_model(crate::session::SessionModelSwitch {
                    sampling_config: incoming,
                    use_concise: false,
                    is_family_switch: false,
                    apply_prompt_override: false,
                    skip_prompt_rewrite: true,
                    auto_compact_threshold_percent: 85,
                    system_prompt_label: distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_owned(),
                })
                .await
                .expect("model switch succeeds");

            let switched = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("model switch keeps sampling config");
            assert_eq!(
                switched.conversation_group_id,
                Some(expected_group),
                "a model config without a group must not erase the session's existing group",
            );
        })
        .await;
}

/// Regression: `handle_set_session_model` must invalidate the memo even when `model_id` is unchanged.
/// Otherwise a config edit that turns the current model into a per-model BYOK model on a third-party `base_url` keeps serving the stale `NotByok`.
/// That leaves the gate active and leaks the OIDC token to the third-party host.
#[tokio::test(flavor = "current_thread")]
async fn set_session_model_invalidates_byok_memo_for_same_model_id() {
    use crate::agent::auth_method::ModelByok;
    use crate::agent::config::ModelAuthFacts;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _rx) = make_actor_with_method_and_credentials(
                None,
                "cached_token",
                distill_chat_state::AuthType::SessionToken,
                "k".to_string(),
            )
            .await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();

            actor
                .model_auth_memo
                .replace(Some(crate::session::acp_session::ModelAuthMemo {
                    model_id: model.clone(),
                    facts: ModelAuthFacts {
                        byok: ModelByok::NotByok,
                        auth_scheme: Default::default(),
                    },
                    provider: None,
                }));

            // Switch to the same model_id, now a per-model BYOK model on a third-party endpoint
            let cfg = distill_sampler::SamplerConfig {
                api_key: Some("byok-key".to_string()),
                base_url: "https://third-party.example/v1".to_string(),
                model: model.clone(),
                context_window: 256_000,
                max_retries: Some(6),
                rate_limit_retry_threshold: Some(4),
                ..Default::default()
            };
            let _ = actor
                .handle_set_session_model(crate::session::SessionModelSwitch {
                    sampling_config: cfg,
                    use_concise: false,
                    is_family_switch: false,
                    apply_prompt_override: false,
                    skip_prompt_rewrite: true,
                    auto_compact_threshold_percent: 85,
                    system_prompt_label: distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_owned(),
                })
                .await;

            let expected_max_retries = distill_sampler::resolve_max_retries(Some(6));
            let stored = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("model switch stores sampling config");
            assert_eq!(stored.max_retries, Some(expected_max_retries));
            assert_eq!(stored.rate_limit_retry_threshold, Some(4));
            assert!(
                actor.model_auth_memo.borrow().is_none(),
                "a model switch must invalidate the per-model BYOK memo so the next \
                 reconstruct recomputes under the current config"
            );
            let reconstructed = actor.reconstruct_full_config().await;
            assert_eq!(reconstructed.max_retries, Some(expected_max_retries));
            assert_eq!(reconstructed.rate_limit_retry_threshold, Some(4));
        })
        .await;
}

use distill_login::test_counting_provider as counting_provider;

/// Seed the per-model memo so `model_auth_provider` resolves without a config load.
async fn seed_provider_memo(actor: &Arc<SessionActor>, provider: distill_login::AuthProviderRef) {
    let model = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .map(|c| c.model)
        .unwrap_or_default();
    actor
        .model_auth_memo
        .replace(Some(crate::session::acp_session::ModelAuthMemo {
            model_id: model,
            facts: crate::agent::config::ModelAuthFacts {
                byok: crate::agent::auth_method::ModelByok::Byok,
                auth_scheme: Default::default(),
            },
            provider: Some(provider),
        }));
}

/// Regression: switching from a provider-backed model to a first-party model must drop the minted provider token from the chat credentials.
/// The token must never go out on a later request to `api.x.ai`.
/// Mirrors the forward direction in `set_session_model_invalidates_byok_memo_for_same_model_id`.
#[tokio::test(flavor = "current_thread")]
async fn switch_to_first_party_model_drops_minted_provider_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("hall-pass", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();
            assert_eq!(token, "tok-1");

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                distill_chat_state::AuthType::ApiKey,
                token,
            )
            .await;
            seed_provider_memo(&actor, provider).await;

            let model = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();

            let cfg = distill_sampler::SamplerConfig {
                api_key: Some("session-jwt".to_string()),
                base_url: "https://api.x.ai/v1".to_string(),
                model,
                context_window: 256_000,
                ..Default::default()
            };
            let _ = actor
                .handle_set_session_model(crate::session::SessionModelSwitch {
                    sampling_config: cfg,
                    use_concise: false,
                    is_family_switch: false,
                    apply_prompt_override: false,
                    skip_prompt_rewrite: true,
                    auto_compact_threshold_percent: 85,
                    system_prompt_label: distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_owned(),
                })
                .await;

            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key.as_deref(),
                Some("session-jwt"),
                "switching to a first-party model must install the session credential, \
                 not the minted provider token"
            );
        })
        .await;
}

/// Arm 4c: a 401 on a provider-backed model re-mints once and resubmits.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_provider_model_remints_and_resubmits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-recover", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();
            assert_eq!(token, "tok-1");

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                distill_chat_state::AuthType::ApiKey,
                token,
            )
            .await;
            seed_provider_memo(&actor, provider).await;
            distill_login::test_backdate_provider_mint(
                "test-4c-recover",
                std::time::Duration::from_secs(60),
            );

            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        store: RecoveredStore::AuthProvider,
                        ..
                    })
                ),
                "provider 401 must re-mint and resubmit via the provider store"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key.as_deref(),
                Some("tok-2"),
                "chat-state credentials must carry the re-minted token"
            );
        })
        .await;
}

/// Arm 4c also fires for a bare 401 that did not classify as `Auth`-kind.
#[tokio::test(flavor = "current_thread")]
async fn sampler_non_auth_kind_401_on_provider_model_still_recovers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-non-auth-kind", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                distill_chat_state::AuthType::ApiKey,
                token,
            )
            .await;
            seed_provider_memo(&actor, provider).await;
            distill_login::test_backdate_provider_mint(
                "test-4c-non-auth-kind",
                std::time::Duration::from_secs(60),
            );

            let mut error = auth_error();
            error.kind = distill_sampler::SamplingErrorKind::Api;
            let result = actor
                .handle_sampling_failure(
                    error,
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "a non-Auth-kind 401 on a provider model must still recover via 4c"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key.as_deref(), Some("tok-2"));
        })
        .await;
}

/// A 401 on a request that went out with no key mints instead of recovering.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_with_no_key_on_provider_model_mints_and_resubmits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-no-key", dir.path());

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                distill_chat_state::AuthType::ApiKey,
                "placeholder".to_string(),
            )
            .await;
            let mut creds = actor.chat_state_handle.get_credentials().await;
            creds.api_key = None;
            actor.chat_state_handle.update_credentials(creds);
            seed_provider_memo(&actor, provider).await;

            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "an unauthenticated 401 on a provider model must mint and resubmit"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key.as_deref(), Some("tok-1"));
        })
        .await;
}

/// A provider model's 401 goes through the provider, never the session refresher (4a/4b vs 4c exclusivity).
/// The actor uses a session-based method, so the gate would be active for a non-BYOK model.
/// The BYOK memo overrides it; that override is the invariant under test.
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_provider_model_never_refreshes_session() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-exclusive", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                distill_chat_state::AuthType::SessionToken,
                token,
            )
            .await;
            seed_provider_memo(&actor, provider).await;
            distill_login::test_backdate_provider_mint(
                "test-4c-exclusive",
                std::time::Duration::from_secs(60),
            );

            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                matches!(
                    result,
                    Ok(SamplerFailureRecovery::RefreshAuthAndResubmit { .. })
                ),
                "the provider arm must recover"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "session refresh must never fire for a provider-backed model"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(creds.api_key.as_deref(), Some("tok-2"));
        })
        .await;
}

/// The pre-turn mirror of the exclusivity test: a cold cache mints the provider token into chat-state, and the session refresher never fires.
/// The actor uses a session-based method, so the gate would be active for a non-BYOK model; the BYOK memo is what keeps the refresher silent.
#[tokio::test(flavor = "current_thread")]
async fn pre_turn_on_provider_model_never_installs_session_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-preturn-exclusive", dir.path());

            let called = Arc::new(AtomicBool::new(false));
            let refresher: Arc<dyn distill_login::refresh::TokenRefresher> =
                Arc::new(AlwaysSucceedRefresher {
                    called: called.clone(),
                });
            let (_dir, am) = auth_manager_with_refresher(refresher);
            let (actor, _rx) = make_actor_with_method_and_credentials(
                Some(am),
                "cached_token",
                distill_chat_state::AuthType::SessionToken,
                "placeholder".to_string(),
            )
            .await;
            // Cold cache: no key on the wire yet.
            let mut creds = actor.chat_state_handle.get_credentials().await;
            creds.api_key = None;
            actor.chat_state_handle.update_credentials(creds);
            seed_provider_memo(&actor, provider).await;

            actor.refresh_token_if_expired().await;

            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key.as_deref(),
                Some("tok-1"),
                "the cold pre-turn hook must mint the provider token"
            );
            assert!(
                !called.load(Ordering::SeqCst),
                "the session refresher must never fire for a provider-backed model"
            );
        })
        .await;
}

/// A token rejected moments after mint surfaces the 401 (fresh-mint guard).
#[tokio::test(flavor = "current_thread")]
async fn sampler_401_on_fresh_provider_token_surfaces_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = tempfile::tempdir().unwrap();
            let provider = counting_provider("test-4c-guard", dir.path());
            let token = provider.ensure_fresh_token(None).await.rotated().unwrap();

            let (actor, _rx) = make_actor_with_auth_and_credentials(
                None,
                distill_chat_state::AuthType::ApiKey,
                token.clone(),
            )
            .await;
            seed_provider_memo(&actor, provider).await;

            let result = actor
                .handle_sampling_failure(
                    auth_error(),
                    0,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert!(
                result.is_err(),
                "a fresh-minted rejected token must surface the 401, not loop"
            );
            let creds = actor.chat_state_handle.get_credentials().await;
            assert_eq!(
                creds.api_key.as_deref(),
                Some(token.as_str()),
                "credentials must be unchanged when the guard blocks the re-mint"
            );
        })
        .await;
}
