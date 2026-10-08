// Modified for Distill by Samuel Fajreldines, 2026.
//! The signed-in Claude subscription's model catalog.
//!
//! Same shape as [`crate::codex_models`]: an account-scoped, short-lived memory
//! cache that is never written into the Grok catalog. The list comes from the
//! Anthropic Models API with the subscription bearer; when that call fails the
//! account still gets a small built-in list, so signing in always leaves at
//! least one model to pick.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use distill_sampling_types::{ApiBackend, ReasoningEffort, ReasoningEffortOption};
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde::Deserialize;

use crate::agent::config::{ModelEntry, ModelInfo};
use crate::claude_auth::{self, ClaudeAuthIdentity};

const MODELS_BASE_URL: &str = "https://api.anthropic.com/v1";
const CATALOG_TTL: Duration = Duration::from_secs(300);
/// The Messages API rejects a `max_tokens` above the model's limit and the
/// sampler's own default is far higher, so a model whose limit the catalog does
/// not report gets a value every current Claude model accepts.
const FALLBACK_MAX_OUTPUT_TOKENS: u32 = 32_000;
const FALLBACK_CONTEXT_WINDOW: u64 = 200_000;
/// Every effort level the Messages API knows, weakest first.
const ALL_EFFORTS: [ReasoningEffort; 5] = [
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::Xhigh,
    ReasoningEffort::Max,
];
/// The API's own default when a request names no effort.
const DEFAULT_EFFORT: ReasoningEffort = ReasoningEffort::High;
/// Used only when the Models API is unreachable or returns nothing. The effort
/// levels are what the API reported for these models.
const FALLBACK_MODELS: &[(&str, &str, &[ReasoningEffort])] = &[
    ("claude-opus-5-5", "Claude Opus 5.5", &ALL_EFFORTS),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5", &ALL_EFFORTS),
    ("claude-haiku-5-5", "Claude Haiku 5.5", &ALL_EFFORTS),
];

struct Catalog {
    identity: ClaudeAuthIdentity,
    fetched_at: Instant,
    models: IndexMap<String, ModelEntry>,
}

static CATALOG: LazyLock<RwLock<Option<Catalog>>> = LazyLock::new(|| RwLock::new(None));
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn cached_models() -> IndexMap<String, ModelEntry> {
    cached_models_for(claude_auth::load_credentials().ok().flatten())
}

fn cached_models_for(
    credentials: Option<claude_auth::ClaudeCredentials>,
) -> IndexMap<String, ModelEntry> {
    let Some(credentials) = credentials else {
        return IndexMap::new();
    };
    CATALOG
        .read()
        .as_ref()
        .filter(|catalog| catalog.identity == credentials.identity())
        .map(|catalog| catalog.models.clone())
        .unwrap_or_default()
}

/// Refreshes the cache for the signed-in account. A failed fetch still
/// publishes the built-in list before returning the error, so the caller can
/// log it while the picker keeps working.
pub(crate) async fn refresh() -> anyhow::Result<()> {
    let _refresh = REFRESH.lock().await;
    let Some(credentials) = claude_auth::fresh_credentials().await? else {
        *CATALOG.write() = None;
        return Ok(());
    };
    let identity = credentials.identity();
    if CATALOG.read().as_ref().is_some_and(|catalog| {
        catalog.identity == identity && catalog.fetched_at.elapsed() < CATALOG_TTL
    }) {
        return Ok(());
    }
    let (models, outcome) = match fetch_models(MODELS_BASE_URL, &credentials.access_token).await {
        Ok(models) if !models.is_empty() => (models, Ok(())),
        Ok(_) => (fallback_models(), Ok(())),
        Err(error) => (fallback_models(), Err(error)),
    };
    // A login/logout that completed during the request must not publish another account's models.
    if claude_auth::load_credentials()?.is_some_and(|current| current.identity() == identity) {
        *CATALOG.write() = Some(Catalog {
            identity,
            fetched_at: Instant::now(),
            models,
        });
    }
    outcome
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<WireModel>,
}

#[derive(Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    display_name: String,
    max_input_tokens: Option<u64>,
    max_tokens: Option<u32>,
    #[serde(default)]
    capabilities: WireCapabilities,
}

#[derive(Deserialize, Default)]
struct WireCapabilities {
    effort: Option<WireEffortSupport>,
    thinking: Option<WireThinkingSupport>,
}

/// `{"supported": bool}`, the shape every capability flag takes.
#[derive(Deserialize, Default, Clone, Copy)]
struct Flag {
    #[serde(default)]
    supported: bool,
}

#[derive(Deserialize, Default)]
struct WireEffortSupport {
    #[serde(default)]
    supported: bool,
    low: Option<Flag>,
    medium: Option<Flag>,
    high: Option<Flag>,
    xhigh: Option<Flag>,
    max: Option<Flag>,
}

#[derive(Deserialize, Default)]
struct WireThinkingSupport {
    types: Option<WireThinkingTypes>,
}

#[derive(Deserialize, Default)]
struct WireThinkingTypes {
    adaptive: Option<Flag>,
}

impl WireCapabilities {
    /// The effort levels this model accepts, weakest first.
    ///
    /// A level the model does not list is rejected with a 400, so only listed
    /// ones are offered. The request builder turns any chosen effort into
    /// `thinking: adaptive`, which some models reject outright (Opus 4.5 takes
    /// effort only without it), so a model without adaptive thinking gets none.
    fn efforts(&self) -> Vec<ReasoningEffort> {
        let adaptive = self
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.types.as_ref())
            .and_then(|types| types.adaptive)
            .is_some_and(|flag| flag.supported);
        let Some(effort) = self.effort.as_ref().filter(|effort| effort.supported && adaptive)
        else {
            return Vec::new();
        };
        ALL_EFFORTS
            .into_iter()
            .filter(|level| {
                let flag = match level {
                    ReasoningEffort::Low => effort.low,
                    ReasoningEffort::Medium => effort.medium,
                    ReasoningEffort::High => effort.high,
                    ReasoningEffort::Xhigh => effort.xhigh,
                    _ => effort.max,
                };
                flag.is_some_and(|flag| flag.supported)
            })
            .collect()
    }
}

async fn fetch_models(
    base_url: &str,
    access_token: &str,
) -> anyhow::Result<IndexMap<String, ModelEntry>> {
    let response = reqwest::Client::new()
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .query(&[("limit", "100")])
        .timeout(Duration::from_secs(5))
        .bearer_auth(access_token)
        .header("anthropic-version", claude_auth::ANTHROPIC_VERSION)
        .header("anthropic-beta", claude_auth::subscription_beta_header())
        .send()
        .await
        .context("Could not load Claude models")?;
    anyhow::ensure!(
        response.status().is_success(),
        "Claude models returned {}",
        response.status()
    );
    let wire: ModelsResponse = response
        .json()
        .await
        .context("Invalid Claude model catalog")?;
    Ok(convert_models(wire))
}

fn model_entry(
    id: &str,
    display_name: &str,
    context_window: u64,
    max_tokens: u32,
    efforts: &[ReasoningEffort],
) -> ModelEntry {
    let key = format!("claude/{id}");
    let default_effort = [DEFAULT_EFFORT, ReasoningEffort::Medium]
        .into_iter()
        .find(|level| efforts.contains(level))
        .or_else(|| efforts.first().copied());
    let reasoning_efforts: Vec<ReasoningEffortOption> = efforts
        .iter()
        .map(|&value| ReasoningEffortOption {
            id: value.to_string(),
            value,
            label: value.to_string(),
            description: None,
            default: Some(value) == default_effort,
        })
        .collect();
    let name = if display_name.trim().is_empty() {
        id
    } else {
        display_name
    };
    let info = ModelInfo {
        id: Some(key),
        model: id.to_owned(),
        name: Some(format!("{name} (Claude)")),
        base_url: MODELS_BASE_URL.to_owned(),
        api_backend: ApiBackend::Messages,
        max_completion_tokens: Some(max_tokens),
        supports_reasoning_effort: !reasoning_efforts.is_empty(),
        reasoning_effort: default_effort,
        reasoning_efforts,
        context_window: std::num::NonZeroU64::new(context_window)
            .unwrap_or(std::num::NonZeroU64::new(FALLBACK_CONTEXT_WINDOW).unwrap()),
        ..Default::default()
    };
    ModelEntry {
        info,
        mtls_cert_dir: None,
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}

fn convert_models(response: ModelsResponse) -> IndexMap<String, ModelEntry> {
    response
        .data
        .into_iter()
        .filter(|model| !model.id.trim().is_empty())
        .map(|model| {
            let entry = model_entry(
                &model.id,
                &model.display_name,
                model.max_input_tokens.unwrap_or(FALLBACK_CONTEXT_WINDOW),
                model.max_tokens.unwrap_or(FALLBACK_MAX_OUTPUT_TOKENS),
                &model.capabilities.efforts(),
            );
            (format!("claude/{}", model.id), entry)
        })
        .collect()
}

fn fallback_models() -> IndexMap<String, ModelEntry> {
    FALLBACK_MODELS
        .iter()
        .map(|(id, name, efforts)| {
            (
                format!("claude/{id}"),
                model_entry(
                    id,
                    name,
                    FALLBACK_CONTEXT_WINDOW,
                    FALLBACK_MAX_OUTPUT_TOKENS,
                    efforts,
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};
    use tokio::net::TcpListener;

    async fn spawn_models_mock(
        status: axum::http::StatusCode,
        body: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/models",
            get(move |headers: axum::http::HeaderMap| {
                let body = body.clone();
                async move {
                    // The catalog is per account: it must be asked for with the
                    // subscription bearer and the beta that makes it valid.
                    assert_eq!(headers["authorization"], "Bearer token");
                    assert!(
                        headers["anthropic-beta"]
                            .to_str()
                            .unwrap()
                            .contains("oauth-2025-04-20")
                    );
                    (status, axum::Json(body))
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), server)
    }

    #[tokio::test]
    async fn account_catalog_keeps_reported_limits_and_routes_through_the_messages_api() {
        let (url, server) = spawn_models_mock(
            axum::http::StatusCode::OK,
            serde_json::json!({"data": [
                {"type": "model", "id": "claude-test-1", "display_name": "Claude Test",
                 "max_input_tokens": 1000000, "max_tokens": 64000},
                {"type": "model", "id": "claude-bare", "display_name": ""},
                {"type": "model", "id": " "}
            ]}),
        )
        .await;

        let models = fetch_models(&url, "token").await.unwrap();

        assert_eq!(models.len(), 2, "a blank id is not a model");
        let test = &models["claude/claude-test-1"].info;
        assert_eq!(test.model, "claude-test-1");
        assert_eq!(test.api_backend, ApiBackend::Messages);
        assert_eq!(test.context_window.get(), 1_000_000);
        assert_eq!(test.max_completion_tokens, Some(64_000));
        assert_eq!(test.name.as_deref(), Some("Claude Test (Claude)"));
        assert_eq!(test.base_url, MODELS_BASE_URL);
        // A model with no reported limits must not inherit the sampler's 128k default.
        let bare = &models["claude/claude-bare"].info;
        assert_eq!(bare.max_completion_tokens, Some(FALLBACK_MAX_OUTPUT_TOKENS));
        assert_eq!(bare.context_window.get(), FALLBACK_CONTEXT_WINDOW);
        assert_eq!(bare.name.as_deref(), Some("claude-bare (Claude)"));
        server.abort();
    }

    fn caps(levels: &[&str], adaptive: bool) -> serde_json::Value {
        let flag = |on: bool| serde_json::json!({"supported": on});
        let level = |name: &str| flag(levels.contains(&name));
        serde_json::json!({
            "effort": {"supported": !levels.is_empty(), "low": level("low"), "medium": level("medium"),
                       "high": level("high"), "xhigh": level("xhigh"), "max": level("max")},
            "thinking": {"supported": true, "types": {"enabled": flag(true), "adaptive": flag(adaptive)}}
        })
    }

    fn effort_ids(entry: &ModelEntry) -> Vec<String> {
        entry.info.reasoning_efforts.iter().map(|o| o.id.clone()).collect()
    }

    #[test]
    fn effort_levels_are_exactly_the_ones_each_model_accepts() {
        // Shapes taken from the live Models API. Offering a level the model
        // lacks is a 400 (`xhigh` on Sonnet 4.6), and adaptive thinking, which
        // the request builder adds with any effort, is a 400 on Opus 4.5 and Haiku 4.5.
        let all = ["low", "medium", "high", "xhigh", "max"];
        let wire = |id: &str, capabilities: serde_json::Value| {
            serde_json::json!({"id": id, "display_name": id, "capabilities": capabilities})
        };
        let response: ModelsResponse = serde_json::from_value(serde_json::json!({"data": [
            wire("sonnet-new", caps(&all, true)),
            wire("sonnet-4-6", caps(&["low", "medium", "high", "max"], true)),
            wire("opus-4-5", caps(&["low", "medium", "high"], false)),
            wire("haiku-4-5", caps(&[], false)),
            {"id": "no-capabilities"}
        ]}))
        .unwrap();

        let models = convert_models(response);

        let new = &models["claude/sonnet-new"];
        assert_eq!(effort_ids(new), ["low", "medium", "high", "xhigh", "max"]);
        assert!(new.info.supports_reasoning_effort);
        // The API's own default, so an unspecified effort behaves as it would without Distill.
        assert_eq!(new.info.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(
            new.info.reasoning_efforts.iter().filter(|o| o.default).map(|o| o.id.as_str()).collect::<Vec<_>>(),
            ["high"]
        );
        assert_eq!(effort_ids(&models["claude/sonnet-4-6"]), ["low", "medium", "high", "max"]);
        for none in ["claude/opus-4-5", "claude/haiku-4-5", "claude/no-capabilities"] {
            let info = &models[none].info;
            assert!(info.reasoning_efforts.is_empty(), "{none}");
            assert!(!info.supports_reasoning_effort, "{none}");
            assert_eq!(info.reasoning_effort, None, "{none}");
        }
    }

    #[test]
    fn a_model_without_a_high_level_defaults_to_medium() {
        let response: ModelsResponse = serde_json::from_value(serde_json::json!({"data": [
            {"id": "m", "capabilities": caps(&["low", "medium"], true)}
        ]}))
        .unwrap();
        let models = convert_models(response);
        assert_eq!(models["claude/m"].info.reasoning_effort, Some(ReasoningEffort::Medium));
    }

    #[test]
    fn built_in_list_offers_efforts_only_where_the_api_reported_them() {
        let models = fallback_models();
        assert_eq!(
            effort_ids(&models["claude/claude-sonnet-5-5"]),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(
            effort_ids(&models["claude/claude-haiku-5-5"]),
            ["low", "medium", "high", "xhigh", "max"]
        );
    }

    #[tokio::test]
    async fn rejected_catalog_request_is_an_error_the_caller_can_fall_back_from() {
        let (url, server) = spawn_models_mock(
            axum::http::StatusCode::UNAUTHORIZED,
            serde_json::json!({"error": {"type": "authentication_error"}}),
        )
        .await;

        let error = fetch_models(&url, "token").await.unwrap_err();

        assert!(error.to_string().contains("401"), "{error}");
        server.abort();
    }

    #[test]
    fn built_in_list_is_selectable_and_uses_the_same_keys_as_the_live_catalog() {
        let models = fallback_models();
        assert!(!models.is_empty());
        for (key, entry) in &models {
            assert!(key.starts_with("claude/"), "{key}");
            assert_eq!(entry.info.id.as_deref(), Some(key.as_str()));
            assert!(!entry.info.hidden);
            assert_eq!(entry.info.api_backend, ApiBackend::Messages);
            assert!(entry.info.max_completion_tokens.is_some());
        }
    }

    #[test]
    #[serial_test::serial]
    fn cached_models_belong_to_the_signed_in_account_only() {
        let account = |uuid: &str| claude_auth::ClaudeCredentials {
            access_token: "token".to_owned(),
            account_uuid: Some(uuid.to_owned()),
            email: None,
        };
        *CATALOG.write() = Some(Catalog {
            identity: account("account-a").identity(),
            fetched_at: Instant::now(),
            models: fallback_models(),
        });

        // Signed out: nothing is offered, even if a catalog is still in memory.
        assert!(cached_models_for(None).is_empty());
        assert!(!cached_models_for(Some(account("account-a"))).is_empty());
        // A re-login as someone else must not surface the previous account's list.
        assert!(cached_models_for(Some(account("account-b"))).is_empty());
        *CATALOG.write() = None;
    }
}
