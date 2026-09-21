//! Import public OpenRouter model metadata into the user's model catalog.

use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use distill_sampling_types::{ReasoningEffort, reasoning_budget_tokens};
use serde_json::{Value, json};

const API: &str = "https://openrouter.ai/api/v1";

pub(crate) async fn import(slug: &str) -> Result<String> {
    import_from(API, slug, &crate::util::distill_home::distill_home()).await
}

async fn import_from(api: &str, slug: &str, home: &Path) -> Result<String> {
    ensure!(
        slug.split_once('/')
            .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
            && !slug
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == ','),
        "Usage: /openrouter <provider/model> (for example: /openrouter z-ai/glm-5.3-flash)"
    );
    let catalog: Value = crate::http::shared_client()
        .get(format!("{api}/models"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("Could not fetch the OpenRouter model catalog")?
        .error_for_status()?
        .json()
        .await
        .context("Invalid OpenRouter model catalog")?;
    let models = catalog["data"]
        .as_array()
        .context("OpenRouter returned no model catalog")?;
    let model = models
        .iter()
        .find(|model| model["id"].as_str() == Some(slug))
        .with_context(|| format!("Model `{slug}` was not found in the OpenRouter catalog"))?;
    let fields = model_config(model)?;
    let slug = slug.to_owned();
    let home = home.to_owned();
    tokio::task::spawn_blocking(move || save_model(&home, &slug, fields))
        .await
        .context("OpenRouter model save task failed")?
}

fn model_config(model: &Value) -> Result<toml::Table> {
    let slug = model["id"].as_str().context("Missing model ID")?;
    ensure!(
        model["architecture"]["output_modalities"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item == "text")),
        "`{slug}` is not a text-output model"
    );
    let context = model["context_length"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("Missing model context window")?;
    let context = model["top_provider"]["context_length"]
        .as_u64()
        .filter(|n| *n > 0)
        .map_or(context, |n| n.min(context));
    let reasoning = &model["reasoning"];
    let budget =
        reasoning["supports_max_tokens"] == true && reasoning.get("supported_efforts").is_none();
    let mandatory = reasoning["mandatory"] == true;
    let mut levels: Vec<ReasoningEffort> = match reasoning.get("supported_efforts") {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    .with_context(|| {
                        format!("Unsupported reasoning effort in OpenRouter metadata: {value}")
                    })
            })
            .collect::<Result<_>>()?,
        None if !budget => vec![],
        Some(Value::Null) | None => {
            vec![
                ReasoningEffort::Minimal,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Xhigh,
                ReasoningEffort::Max,
            ]
        }
        _ => bail!("Invalid reasoning efforts in OpenRouter metadata"),
    };
    // Budget mode cannot disable thinking: its zero budget means no override.
    levels.retain(|level| *level != ReasoningEffort::None || (!mandatory && !budget));
    if !mandatory && !budget && !levels.is_empty() && !levels.contains(&ReasoningEffort::None) {
        levels.push(ReasoningEffort::None);
    }
    let mut unique = Vec::new();
    for level in levels {
        if !unique.contains(&level) {
            unique.push(level);
        }
    }
    let default = (reasoning["default_enabled"] == false
        && unique.contains(&ReasoningEffort::None))
    .then_some(ReasoningEffort::None)
    .or_else(|| {
        reasoning["default_effort"]
            .as_str()
            .and_then(|value| value.parse::<ReasoningEffort>().ok())
            .filter(|value| unique.contains(value))
    })
    .or_else(|| {
        unique
            .contains(&ReasoningEffort::Medium)
            .then_some(ReasoningEffort::Medium)
    })
    .or_else(|| unique.first().copied());
    let options: Vec<_> = unique
        .into_iter()
        .map(|level| {
            let mut option = json!({
                "id": level,
                "value": level,
                "label": level,
                "default": Some(level) == default,
            });
            if budget {
                option["description"] = json!(format!(
                    "Reasoning budget: {} tokens",
                    reasoning_budget_tokens(level)
                ));
            }
            option
        })
        .collect();
    let mut fields = json!({
        "model": slug,
        "name": model["name"].as_str().unwrap_or(slug),
        "base_url": API,
        "api_backend": "chat_completions",
        "context_window": context,
        "supports_reasoning_effort": !options.is_empty(),
        "reasoning_efforts": options,
        "reasoning_shape": if budget { "max_tokens" } else { "effort" },
    });
    if let Some(default) = default {
        fields["reasoning_effort"] = json!(default);
    }
    if let Some(description) = model["description"].as_str() {
        fields["description"] = json!(description);
    }
    if let Some(limit) = model["top_provider"]["max_completion_tokens"]
        .as_u64()
        .filter(|limit| *limit > 0 && *limit <= u32::MAX as u64)
    {
        fields["max_completion_tokens"] = json!(limit.min(context));
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = model["default_parameters"][key].as_f64() {
            fields[key] = json!(value);
        }
    }
    let fields: toml::Table = serde_json::from_value(fields)?;
    // Use the same schema the runtime loads before touching the user's file.
    let _: crate::agent::config::ModelEntryConfig =
        toml::Value::Table(fields.clone()).try_into()?;
    Ok(fields)
}

fn save_model(home: &Path, slug: &str, fields: toml::Table) -> Result<String> {
    let mut key = format!("openrouter/{slug}");
    crate::config::update_config_toml_locked(home, |config| {
        let models = config
            .entry("model")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or("[model] is not a table")?;
        // Refresh an existing OpenRouter entry without creating a second picker row.
        if let Some((existing, _)) = models.iter().find(|(_, entry)| {
            entry.get("model").and_then(toml::Value::as_str) == Some(slug)
                && entry
                    .get("base_url")
                    .and_then(toml::Value::as_str)
                    .is_some_and(crate::openrouter_auth::is_openrouter_url)
        }) {
            key = existing.clone();
        }
        if let Some(entry) = models.get(&key) {
            if entry.get("model").and_then(toml::Value::as_str) != Some(slug)
                || !entry
                    .get("base_url")
                    .and_then(toml::Value::as_str)
                    .is_some_and(crate::openrouter_auth::is_openrouter_url)
            {
                return Err(format!("Model key `{key}` already belongs to another model").into());
            }
        }
        let entry = models
            .entry(key.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or("Model entry is not a table")?;
        let before = entry.clone();
        for field in [
            "reasoning_effort",
            "reasoning_efforts",
            "reasoning_shape",
            "max_completion_tokens",
        ] {
            entry.remove(field);
        }
        entry.extend(fields);
        if !entry.contains_key("api_key") && !entry.contains_key("auth_provider") {
            entry
                .entry("env_key")
                .or_insert_with(|| toml::Value::String(crate::openrouter_auth::API_KEY_ENV.into()));
        }
        Ok(*entry != before)
    })
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Value {
        json!({"id":"z-ai/glm-test", "name":"GLM Test", "context_length":1310720,
            "architecture":{"output_modalities":["text"]},
            "top_provider":{"context_length":1048576,"max_completion_tokens":131072},
            "reasoning":{"mandatory":true,"supported_efforts":["max","high","low"],"default_effort":"max"}})
    }

    #[tokio::test]
    async fn imports_catalog_and_refreshes_existing_entry_without_changing_other_settings() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        std::fs::write(&path, "[jev]\neffort_auto = true\n[model.existing]\nmodel = 'z-ai/glm-test'\nbase_url = 'https://openrouter.ai/api/v1'\nenv_key = 'CUSTOM_OPENROUTER_KEY'\ncontext_window = 100\n").unwrap();
        let catalog = json!({"data":[model()]});
        let app = axum::Router::new().route(
            "/models",
            axum::routing::get(move || async move { axum::Json(catalog) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert_eq!(
            import_from(&api, "z-ai/glm-test", home.path())
                .await
                .unwrap(),
            "existing"
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        let config: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(config["jev"]["effort_auto"].as_bool(), Some(true));
        assert_eq!(config["model"].as_table().unwrap().len(), 1);
        let entry = &config["model"]["existing"];
        assert_eq!(entry["env_key"].as_str(), Some("CUSTOM_OPENROUTER_KEY"));
        assert_eq!(entry["context_window"].as_integer(), Some(1048576));
        assert_eq!(entry["reasoning_effort"].as_str(), Some("max"));
        let options = entry["reasoning_efforts"].as_array().unwrap();
        assert_eq!(
            options
                .iter()
                .map(|v| v["value"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["max", "high", "low"]
        );
        import_from(&api, "z-ai/glm-test", home.path())
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        assert!(
            import_from(&api, "z-ai/missing", home.path())
                .await
                .is_err()
        );
        assert!(import_from(&api, "", home.path()).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        server.abort();
    }

    #[test]
    fn budget_menu_does_not_claim_to_disable_thinking_and_nonreasoning_has_no_menu() {
        let mut model = model();
        model["reasoning"] = json!({"mandatory":false,"supports_max_tokens":true});
        let fields = model_config(&model).unwrap();
        assert_eq!(fields["reasoning_shape"].as_str(), Some("max_tokens"));
        let options = fields["reasoning_efforts"].as_array().unwrap();
        assert_eq!(options.len(), 6);
        assert!(options.iter().all(|v| v["value"].as_str() != Some("none")));
        model.as_object_mut().unwrap().remove("reasoning");
        let fields = model_config(&model).unwrap();
        assert_eq!(fields["supports_reasoning_effort"].as_bool(), Some(false));
        assert!(fields["reasoning_efforts"].as_array().unwrap().is_empty());
    }
}
