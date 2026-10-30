use super::*;
use crate::accounts::AutoRouting;
use distill_test_support::EnvGuard;
use std::future::IntoFuture;

/// Re-exec like spawn_runtime_containment_tests: home is process-cached and actor futures need the session-size stack.
pub(crate) fn isolated(test: &str, body: impl FnOnce()) {
    const CHILD: &str = "DISTILL_ACCOUNT_ROUTING_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        body();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .env("RUST_MIN_STACK", "16777216")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("XAI_API_KEY")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated account test failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "exact filter matched no test"
    );
}

pub(crate) struct NamedAccounts {
    pub store: AccountStore,
    pub models: IndexMap<String, ModelEntry>,
    _home: tempfile::TempDir,
    _env: Vec<EnvGuard>,
}
impl NamedAccounts {
    pub(crate) fn new(endpoint: &str) -> Self {
        use base64::Engine;
        let home = tempfile::tempdir().unwrap();
        let env = vec![
            EnvGuard::set("GROK_HOME", home.path()),
            EnvGuard::set("DISTILL_HOME", home.path()),
            EnvGuard::unset("OPENAI_API_KEY"),
            EnvGuard::unset("OPENROUTER_API_KEY"),
            EnvGuard::unset("XAI_API_KEY"),
            EnvGuard::set(crate::codex_auth::CODEX_INFERENCE_BASE_URL_ENV, endpoint),
            EnvGuard::set("GROK_CODEX_BACKEND_BASE_URL", endpoint),
        ];
        let store = AccountStore::new(home.path().to_owned());
        let mut models = IndexMap::new();
        for id in ["gpt-6.1-sol", "gpt-6-luna"] {
            let mut entry =
                ModelEntry::fallback(id, &super::super::config::EndpointsConfig::default());
            entry.info.base_url = endpoint.into();
            entry.info.api_backend = distill_sampling_types::ApiBackend::Responses;
            entry.info.max_retries = Some(0);
            entry.info.context_window = std::num::NonZeroU64::new(262_144).unwrap();
            entry.info.max_completion_tokens = Some(4096);
            models.insert(id.into(), entry);
        }
        for id in ["one", "two", "three"] {
            store.add(id, Provider::Chatgpt, id).unwrap();
            let path = store.credential_path(id).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({
                "exp": Utc::now().timestamp()+3600,
                "https://api.openai.com/auth": {"chatgpt_account_id": format!("wire-{id}"), "chatgpt_user_id": format!("user-{id}")}
            })).unwrap());
            std::fs::write(
                path,
                serde_json::to_vec(&crate::codex_auth::CodexAuthStore {
                    auth_mode: Some("chatgpt".into()),
                    openai_api_key: None,
                    tokens: Some(crate::codex_auth::CodexTokenData {
                        id_token: format!("e30.{payload}.signature"),
                        access_token: format!("{id}-token"),
                        refresh_token: format!("{id}-refresh"),
                        account_id: Some(format!("wire-{id}")),
                    }),
                    last_refresh: Some(Utc::now()),
                })
                .unwrap(),
            )
            .unwrap();
        }
        let result = Self {
            store,
            models,
            _home: home,
            _env: env,
        };
        for id in ["one", "two", "three"] {
            result.quota(id, Some(50.0));
        }
        result.routing(AutoStrategy::OrderedFirst, true);
        result
    }
    pub(crate) fn quota(&self, id: &str, remaining: Option<f64>) {
        let snapshot = QuotaSnapshot {
            account_id: id.into(),
            checked_at: Utc::now(),
            fetched_at: remaining.map(|_| Utc::now()),
            status: if remaining.is_some() {
                QuotaStatus::Available
            } else {
                QuotaStatus::Unsupported
            },
            remaining_percent: remaining,
            detail: None,
        };
        std::fs::write(
            self._home
                .path()
                .join("accounts")
                .join(id)
                .join("quota.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
    }
    pub(crate) fn routing(&self, strategy: AutoStrategy, enabled: bool) {
        self.store
            .save_routing(&AutoRouting {
                enabled,
                strategy,
                options: ["one", "two", "three"]
                    .into_iter()
                    .map(|id| AutoOption {
                        id: id.into(),
                        account_id: id.into(),
                        main_model: if id == "one" {
                            "gpt-6.1-sol"
                        } else {
                            "gpt-6-luna"
                        }
                        .into(),
                        worker_model: Some(
                            if id == "one" {
                                "gpt-6-luna"
                            } else {
                                "gpt-6.1-sol"
                            }
                            .into(),
                        ),
                    })
                    .collect(),
            })
            .unwrap();
    }
    pub(crate) fn add_expired_claude(&mut self) {
        self.store
            .add("expired", Provider::Claude, "Expired Claude")
            .unwrap();
        let path = self.store.credential_path("expired").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&crate::claude_auth::ClaudeAuthStore {
                access_token: "expired-access".into(),
                refresh_token: String::new(),
                expires_at: Utc::now() - Duration::hours(1),
                scopes: Vec::new(),
                account_uuid: Some("expired-account".into()),
                email: None,
                organization_uuid: None,
            })
            .unwrap(),
        )
        .unwrap();
        self.quota("expired", None);
        let mut claude = self.models["gpt-6.1-sol"].clone();
        claude.info.model = "claude-opus-5-5".into();
        claude.info.base_url = "https://api.anthropic.com".into();
        self.models.insert("claude/claude-opus-5-5".into(), claude);
        let mut routing = self.store.load_routing().unwrap();
        routing.options.insert(
            0,
            AutoOption {
                id: "expired".into(),
                account_id: "expired".into(),
                main_model: "claude/claude-opus-5-5".into(),
                worker_model: None,
            },
        );
        self.store.save_routing(&routing).unwrap();
    }
    pub(crate) fn base(&self) -> SamplerConfig {
        SamplerConfig {
            model: "gpt-6.1-sol".into(),
            base_url: crate::codex_auth::inference_base_url(),
            api_backend: distill_sampling_types::ApiBackend::Responses,
            context_window: 262_144,
            ..Default::default()
        }
    }
}

#[test]
#[serial_test::serial]
fn account_routing_order_unknown_highest_and_disabled_account_selection() {
    crate::agent::account_routing::tests::isolated(
        "agent::account_routing::tests::account_routing_order_unknown_highest_and_disabled_account_selection",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                // Legacy model resolution recognizes the canonical endpoint; fresh
                // credentials and quota snapshots keep this selector-only test offline.
                let fixture = NamedAccounts::new(crate::codex_auth::CODEX_INFERENCE_BASE_URL);
                fixture.quota("one", None);
                fixture.quota("two", Some(30.0));
                fixture.quota("three", Some(80.0));
                fixture
                    .store
                    .add("offline", Provider::Claude, "Offline Claude")
                    .unwrap();
                let mut routing = fixture.store.load_routing().unwrap();
                routing.options.insert(
                    0,
                    AutoOption {
                        id: "offline".into(),
                        account_id: "offline".into(),
                        main_model: "claude/unavailable-catalog-model".into(),
                        worker_model: None,
                    },
                );
                fixture.store.save_routing(&routing).unwrap();
                let ordered =
                    routes_for_turn(&fixture.store, &fixture.models, &fixture.base(), true)
                        .await
                        .unwrap();
                assert_eq!(
                    ordered
                        .iter()
                        .map(|r| r.account_id.as_str())
                        .collect::<Vec<_>>(),
                    ["one", "two", "three"]
                );
                fixture.routing(AutoStrategy::HighestRemaining, true);
                let highest = select_startup_config(&fixture.base(), &fixture.models, false)
                    .await
                    .unwrap();
                assert_eq!(
                    AccountRoute::read(&highest.extra_headers)
                        .unwrap()
                        .unwrap()
                        .account_id,
                    "three"
                );
                assert_eq!(
                    highest
                        .bearer_resolver
                        .as_ref()
                        .unwrap()
                        .current_bearer()
                        .as_deref(),
                    Some("three-token")
                );
                fixture.routing(AutoStrategy::HighestRemaining, false);
                let disabled = select_startup_config(&fixture.base(), &fixture.models, false)
                    .await
                    .unwrap();
                assert_eq!(
                    disabled.model, "gpt-6.1-sol",
                    "Auto disabled keeps model choice"
                );
                assert_eq!(
                    disabled
                        .bearer_resolver
                        .unwrap()
                        .current_bearer()
                        .as_deref(),
                    Some("three-token")
                );
                let legacy_path = fixture.store.credential_path("legacy-chatgpt").unwrap();
                std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
                std::fs::copy(fixture.store.credential_path("one").unwrap(), legacy_path).unwrap();
                std::fs::create_dir_all(fixture._home.path().join("accounts/legacy-chatgpt"))
                    .unwrap();
                fixture.quota("legacy-chatgpt", None);
                let subscription = &fixture.models["gpt-6.1-sol"];
                let legacy = crate::agent::config::sampling_config_for_model(
                    subscription,
                    crate::agent::config::resolve_credentials(subscription, None),
                    None,
                    None,
                    None,
                    None,
                );
                assert!(legacy.api_key.is_none());
                assert!(
                    !legacy
                        .extra_headers
                        .contains_key(CREDENTIAL_AUTHORITY_HEADER)
                );
                assert_eq!(
                    legacy
                        .bearer_resolver
                        .as_ref()
                        .unwrap()
                        .current_bearer()
                        .as_deref(),
                    Some("one-token")
                );
                let ranked = select_startup_config(&legacy, &fixture.models, true)
                    .await
                    .unwrap();
                assert_eq!(
                    AccountRoute::read(&ranked.extra_headers)
                        .unwrap()
                        .unwrap()
                        .account_id,
                    "three"
                );
                assert_eq!(
                    ranked.bearer_resolver.unwrap().current_bearer().as_deref(),
                    Some("three-token")
                );

                // The earlier subscription entry and the later BYOK alias share a wire slug.
                let mut byok_models = fixture.models.clone();
                let mut alias = subscription.clone();
                alias.info.id = Some("my-byok".into());
                alias.api_key = Some("explicit-key".into());
                alias
                    .info
                    .extra_headers
                    .insert("Authorization".into(), "Bearer explicit-key".into());
                alias
                    .info
                    .env_http_headers
                    .insert("x-api-key".into(), "MY_PROVIDER_KEY".into());
                byok_models.insert("my-byok".into(), alias);
                let alias = &byok_models["my-byok"];
                let byok = crate::agent::config::sampling_config_for_model(
                    alias,
                    crate::agent::config::resolve_credentials(alias, None),
                    None,
                    None,
                    None,
                    None,
                );
                assert_eq!(byok.model, "gpt-6.1-sol");
                assert!(
                    find_model_by_id(&byok_models, &byok.model)
                        .unwrap()
                        .api_key
                        .is_none()
                );
                let unchanged = select_startup_config(&byok, &byok_models, false)
                    .await
                    .unwrap();
                assert_eq!(unchanged.api_key, byok.api_key);
                assert_eq!(unchanged.extra_headers, byok.extra_headers);
                assert_eq!(unchanged.env_http_headers, byok.env_http_headers);
                assert!(unchanged.bearer_resolver.is_none());
                fixture.routing(AutoStrategy::OrderedFirst, true);
                let manual = select_startup_config(&byok, &byok_models, true)
                    .await
                    .unwrap();
                assert_eq!(manual.api_key, byok.api_key);
                assert!(AccountRoute::read(&manual.extra_headers).unwrap().is_none());
                let explicit_auto = explicit_auto_config(&byok, &byok_models).await.unwrap();
                assert!(explicit_auto.api_key.is_none());
                assert_eq!(
                    AccountRoute::read(&explicit_auto.extra_headers)
                        .unwrap()
                        .unwrap()
                        .account_id,
                    "one"
                );
            });
        },
    );
}

#[test]
#[serial_test::serial]
fn account_routing_startup_skips_auto_account_that_cannot_authenticate() {
    isolated(
        "agent::account_routing::tests::account_routing_startup_skips_auto_account_that_cannot_authenticate",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let mut fixture = NamedAccounts::new(crate::codex_auth::CODEX_INFERENCE_BASE_URL);
                fixture.quota("one", Some(0.0));
                fixture.add_expired_claude();
                let routing = fixture.store.load_routing().unwrap();
                assert!(fixture.store.status("expired", None).unwrap().connected);
                let routes =
                    routes_for_turn(&fixture.store, &fixture.models, &fixture.base(), true)
                        .await
                        .unwrap();
                assert_eq!(routes[0].account_id, "expired");
                assert!(
                    config_for_route(&fixture.store, &fixture.models, &fixture.base(), &routes[0])
                        .await
                        .is_err()
                );

                let selected = explicit_auto_config(&fixture.base(), &fixture.models)
                    .await
                    .expect("Auto must try the next configured account after authentication fails");
                let route = AccountRoute::read(&selected.extra_headers)
                    .unwrap()
                    .unwrap();
                assert_eq!(route.account_id, "two");
                assert_eq!(route.worker_model, "gpt-6.1-sol");
                let mut prepared =
                    config_for_routes(&fixture.store, &fixture.models, &selected, &routes)
                        .await
                        .expect(
                            "turn preparation must also skip the account that cannot authenticate",
                        );
                reresolve_or_block(&fixture.store, &fixture.models, &mut prepared, true).await;
                assert_eq!(
                    AccountRoute::read(&prepared.extra_headers)
                        .unwrap()
                        .unwrap()
                        .account_id,
                    "two"
                );
                assert!(
                    !prepared
                        .extra_headers
                        .contains_key("x-distill-account-unavailable")
                );
                assert_eq!(
                    selected
                        .bearer_resolver
                        .unwrap()
                        .current_bearer()
                        .as_deref(),
                    Some("two-token")
                );
                assert_eq!(fixture.store.load_routing().unwrap(), routing);

                let pinned = SamplerConfig {
                    model: "claude/claude-opus-5-5".into(),
                    base_url: "https://api.anthropic.com".into(),
                    ..fixture.base()
                };
                let pinned = select_startup_config(&pinned, &fixture.models, true).await;
                assert!(
                    pinned.is_err(),
                    "an explicitly selected account must still fail closed"
                );
                fixture.quota("two", Some(0.0));
                fixture.quota("three", Some(0.0));
                assert!(
                    explicit_auto_config(&fixture.base(), &fixture.models)
                        .await
                        .is_err(),
                    "Auto must fail closed when no configured account can authenticate"
                );
            });
        },
    );
}

#[test]
#[serial_test::serial]
fn account_routing_refreshes_missing_quota_and_reuses_fresh_cache() {
    crate::agent::account_routing::tests::isolated(
        "agent::account_routing::tests::account_routing_refreshes_missing_quota_and_reuses_fresh_cache",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
    use axum::{Json, Router, http::HeaderMap, routing::get};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(axum::serve(listener, Router::new()
        .route("/wham/usage", get(move |headers: HeaderMap| { let calls = count.clone(); async move {
            calls.fetch_add(1, Ordering::Relaxed);
            let used = if headers.get("chatgpt-account-id").unwrap() == "wire-three" { 10 } else { 90 };
            Json(serde_json::json!({"rate_limit":{"primary_window":{"used_percent":used,"limit_window_seconds":3600,"reset_after_seconds":3600,"reset_at":Utc::now().timestamp()+3600},"secondary_window":{"used_percent":0,"limit_window_seconds":604800,"reset_after_seconds":604800,"reset_at":Utc::now().timestamp()+604800}}}))
        }})).route("/wham/profiles/me", get(|| async { Json(serde_json::json!({})) }))).into_future());
    let fixture = NamedAccounts::new(&format!("{url}/v1"));
    let _backend = EnvGuard::set("GROK_CODEX_BACKEND_BASE_URL", &url);
    for id in ["one", "two", "three"] {
        std::fs::remove_file(
            fixture
                ._home
                .path()
                .join("accounts")
                .join(id)
                .join("quota.json"),
        )
        .unwrap();
    }
    fixture.routing(AutoStrategy::HighestRemaining, true);
    let selected = select_startup_config(&fixture.base(), &fixture.models, false)
        .await
        .unwrap();
    assert_eq!(
        AccountRoute::read(&selected.extra_headers)
            .unwrap()
            .unwrap()
            .account_id,
        "three"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    let _ = select_startup_config(&fixture.base(), &fixture.models, false)
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        3,
        "fresh cache avoids quota fetch per round"
    );
    task.abort();

        });
        },
    );
}

#[test]
fn account_routing_claude_pair_quota_and_provider_specific_402() {
    let mut models = IndexMap::new();
    models.insert(
        "main".into(),
        ModelEntry::fallback(
            "claude-sonnet-5-5",
            &super::super::config::EndpointsConfig::default(),
        ),
    );
    models.insert(
        "worker".into(),
        ModelEntry::fallback(
            "claude-opus-5-5",
            &super::super::config::EndpointsConfig::default(),
        ),
    );
    let route = AccountRoute {
        account_id: "claude".into(),
        option_id: Some("pair".into()),
        main_model: "main".into(),
        worker_model: "worker".into(),
        worker: false,
        pinned: false,
        operation_id: None,
        failed_options: vec![],
    };
    let mut candidates = vec![(
        route,
        Some(QuotaSnapshot {
            account_id: "claude".into(),
            checked_at: Utc::now(),
            fetched_at: Some(Utc::now()),
            status: QuotaStatus::Available,
            remaining_percent: Some(80.0),
            detail: Some(serde_json::json!({"seven_day_opus":{"utilization":100}})),
        }),
    )];
    apply_model_quotas(&mut candidates, &models);
    assert!(
        rank_routes(candidates, AutoStrategy::OrderedFirst).is_empty(),
        "known zero Opus worker excludes the whole pair"
    );
    let mut info =
        distill_sampler::SamplingErrorInfo::from(&distill_sampling_types::SamplingError::Api {
            status: reqwest::StatusCode::PAYMENT_REQUIRED,
            message: "insufficient credits".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
        });
    assert!(quota_fallback_eligible(&info, Provider::Openrouter));
    assert!(!quota_fallback_eligible(&info, Provider::Chatgpt));
    info.should_retry = Some(false);
    assert!(
        !quota_fallback_eligible(&info, Provider::Openrouter),
        "output veto wins over 402"
    );
    info.should_retry = None;
    info.status_code = Some(403);
    assert!(!quota_fallback_eligible(&info, Provider::Openrouter));
}

#[test]
#[serial_test::serial]
fn account_routing_reresolves_auto_route_after_identity_change() {
    crate::agent::account_routing::tests::isolated(
        "agent::account_routing::tests::account_routing_reresolves_auto_route_after_identity_change",
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let fixture = NamedAccounts::new(crate::codex_auth::CODEX_INFERENCE_BASE_URL);
                let selected = select_startup_config(&fixture.base(), &fixture.models, false)
                    .await
                    .unwrap();
                // The account was signed in again: its stored anchor no longer matches.
                let mut stale = selected.clone();
                stale.extra_headers.insert(
                    "x-distill-codex-account-anchor".into(),
                    "old-identity".into(),
                );
                assert!(
                    restore_credentials(&fixture.store, &mut stale.clone())
                        .await
                        .is_err()
                );

                // Auto must pick a route again instead of failing every retry.
                let mut auto = stale.clone();
                reresolve_or_block(&fixture.store, &fixture.models, &mut auto, true).await;
                assert!(
                    !auto
                        .extra_headers
                        .contains_key("x-distill-account-unavailable")
                );
                assert_eq!(
                    AccountRoute::read(&auto.extra_headers)
                        .unwrap()
                        .unwrap()
                        .account_id,
                    "one"
                );
                assert_ne!(
                    auto.extra_headers.get("x-distill-codex-account-anchor"),
                    Some(&"old-identity".to_string())
                );
                assert!(account_history_needs_portability(
                    &distill_sampling_types::SamplingConfig {
                        model: stale.model.clone(),
                        base_url: stale.base_url.clone(),
                        api_backend: stale.api_backend.clone(),
                        extra_headers: stale.extra_headers.clone(),
                        ..Default::default()
                    },
                    &auto
                ));

                // An explicit account choice keeps failing closed.
                let mut pinned = stale.clone();
                let mut route = AccountRoute::read(&pinned.extra_headers).unwrap().unwrap();
                route.pinned = true;
                route.write(&mut pinned).unwrap();
                reresolve_or_block(&fixture.store, &fixture.models, &mut pinned, true).await;
                assert!(
                    pinned
                        .extra_headers
                        .contains_key("x-distill-account-unavailable")
                );

                // A routing-locked delegate does not pick a new account either.
                let mut locked = stale.clone();
                reresolve_or_block(&fixture.store, &fixture.models, &mut locked, false).await;
                assert!(
                    locked
                        .extra_headers
                        .contains_key("x-distill-account-unavailable")
                );
            });
        },
    );
}
