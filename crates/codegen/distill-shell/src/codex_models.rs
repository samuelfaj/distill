// Modified for Distill by Samuel Fajreldines, 2026.
//! The signed-in ChatGPT account's model catalog and per-model effort menus.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::Context;
use distill_sampling_types::{
    ApiBackend, ReasoningEffort, ReasoningEffortOption, ReasoningSummary,
};
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde::Deserialize;

use crate::agent::config::{ModelEntry, ModelInfo};
use crate::codex_auth::{self, CodexAuthIdentity};

struct Catalog {
    identity: CodexAuthIdentity,
    base_url: String,
    fetched_at: Instant,
    models: IndexMap<String, ModelEntry>,
}

static CATALOG: LazyLock<RwLock<Option<Catalog>>> = LazyLock::new(|| RwLock::new(None));
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn cached_models() -> IndexMap<String, ModelEntry> {
    let Some(credentials) = codex_auth::load_credentials().ok().flatten() else {
        return IndexMap::new();
    };
    CATALOG
        .read()
        .as_ref()
        .filter(|catalog| {
            catalog.identity == credentials.identity()
                && catalog.base_url == codex_auth::inference_base_url()
        })
        .map(|catalog| catalog.models.clone())
        .unwrap_or_default()
}

/// Account-scoped, short-lived memory cache. Never writes into the Grok catalog.
pub(crate) async fn refresh() -> anyhow::Result<()> {
    let _refresh = REFRESH.lock().await;
    let Some(credentials) = codex_auth::fresh_credentials().await? else {
        *CATALOG.write() = None;
        return Ok(());
    };
    let identity = credentials.identity();
    let base_url = codex_auth::inference_base_url();
    if CATALOG.read().as_ref().is_some_and(|catalog| {
        catalog.identity == identity
            && catalog.base_url == base_url
            && catalog.fetched_at.elapsed() < Duration::from_secs(300)
    }) {
        return Ok(());
    }
    let version = codex_auth::codex_client_version();
    let mut request = reqwest::Client::new()
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .query(&[("client_version", &version)])
        .timeout(Duration::from_secs(5))
        .bearer_auth(&credentials.access_token)
        .header("originator", codex_auth::CODEX_ORIGINATOR)
        .header("version", &version)
        .header(
            reqwest::header::USER_AGENT,
            format!("{}/{version}", codex_auth::CODEX_ORIGINATOR),
        );
    if let Some(account_id) = &credentials.account_id {
        request = request.header("ChatGPT-Account-ID", account_id);
    }
    if credentials.account_is_fedramp {
        request = request.header("X-OpenAI-Fedramp", "true");
    }
    let response = request
        .send()
        .await
        .context("Could not load ChatGPT models")?;
    anyhow::ensure!(
        response.status().is_success(),
        "ChatGPT models returned {}",
        response.status()
    );
    let wire: ModelsResponse = response
        .json()
        .await
        .context("Invalid ChatGPT model catalog")?;
    let models = convert_models(wire, &base_url);
    // A login/logout that completed during the request must not publish another account's models.
    if codex_auth::load_credentials()?.is_some_and(|current| current.identity() == identity)
        && base_url == codex_auth::inference_base_url()
    {
        *CATALOG.write() = Some(Catalog {
            identity,
            base_url,
            fetched_at: Instant::now(),
            models,
        });
    }
    Ok(())
}

#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<WireModel>,
}

#[derive(Deserialize)]
struct WireModel {
    slug: String,
    #[serde(default)]
    display_name: String,
    description: Option<String>,
    visibility: String,
    #[serde(default)]
    priority: i64,
    context_window: Option<u64>,
    default_reasoning_level: Option<ReasoningEffort>,
    #[serde(default)]
    supported_reasoning_levels: Vec<WireEffort>,
    default_reasoning_summary: Option<ReasoningSummary>,
}

#[derive(Deserialize)]
struct WireEffort {
    effort: String,
    description: Option<String>,
}

fn convert_models(mut response: ModelsResponse, base_url: &str) -> IndexMap<String, ModelEntry> {
    response.models.sort_by_key(|model| model.priority);
    response
        .models
        .into_iter()
        .filter(|model| !model.slug.trim().is_empty())
        .map(|model| {
            let key = format!("chatgpt/{}", model.slug);
            let mut efforts: Vec<ReasoningEffortOption> = model
                .supported_reasoning_levels
                .into_iter()
                .filter_map(|level| {
                    let value = level.effort.parse().ok()?;
                    Some(ReasoningEffortOption {
                        id: level.effort.clone(),
                        value,
                        label: level.effort,
                        description: level.description,
                        default: Some(value) == model.default_reasoning_level,
                    })
                })
                .collect();
            if !efforts.iter().any(|option| option.default)
                && let Some(first) = efforts.first_mut()
            {
                first.default = true;
            }
            let name = if model.display_name.trim().is_empty() {
                &model.slug
            } else {
                &model.display_name
            };
            let mut info = ModelInfo {
                id: Some(key.clone()),
                model: model.slug.clone(),
                name: Some(format!("{name} (ChatGPT)")),
                description: model.description,
                base_url: base_url.to_owned(),
                api_backend: ApiBackend::Responses,
                hidden: model.visibility != "list",
                supports_reasoning_effort: !efforts.is_empty(),
                reasoning_effort: efforts
                    .iter()
                    .find(|option| option.default)
                    .map(|option| option.value),
                reasoning_efforts: efforts,
                reasoning_summary: model.default_reasoning_summary,
                ..Default::default()
            };
            if let Some(window) = model.context_window.and_then(std::num::NonZeroU64::new) {
                info.context_window = window;
            }
            (
                key,
                ModelEntry {
                    info,
                    mtls_cert_dir: None,
                    api_key: None,
                    env_key: None,
                    auth_provider: None,
                    api_base_url: None,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_catalog_preserves_visibility_routing_and_per_model_efforts() {
        let response = serde_json::from_value(serde_json::json!({"models": [
            {"slug":"gpt-test", "display_name":"GPT Test", "visibility":"list", "context_window":272000,
             "default_reasoning_level":"medium", "default_reasoning_summary":"none",
             "supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"max"},{"effort":"ultra"}]},
            {"slug":"internal-review", "visibility":"hide", "supported_reasoning_levels":[{"effort":"high"}]}
        ]})).unwrap();
        let models = convert_models(response, codex_auth::CODEX_INFERENCE_BASE_URL);
        let model = &models["chatgpt/gpt-test"].info;
        assert!(!model.hidden);
        assert_eq!(model.model, "gpt-test");
        assert_eq!(model.api_backend, ApiBackend::Responses);
        assert_eq!(model.context_window.get(), 272000);
        assert_eq!(model.reasoning_effort, Some(ReasoningEffort::Medium));
        assert_eq!(model.reasoning_summary, Some(ReasoningSummary::None));
        assert_eq!(
            model
                .reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::Max,
                ReasoningEffort::Ultra
            ]
        );
        assert!(models["chatgpt/internal-review"].info.hidden);
        assert_eq!(
            models["chatgpt/internal-review"]
                .info
                .reasoning_efforts
                .len(),
            1
        );
    }
}
