//! One-shot migration to the main/worker model layout.
//!
//! `[models].default` is the **main** model (`/model`, required): it owns every
//! session and plans, delegates and reviews. `[models].worker` is the optional
//! **worker** model (`/worker-model`) that runs the delegated work.
//!
//! Two earlier layouts are rewritten: `[models].reasoning`, the strong model a
//! cheaper main model consulted, becomes the main model and that cheaper model
//! becomes the worker; `[jev.tiers].light`, the worker of the oldest layout,
//! becomes the worker next to the main model it already had. A fixed effort
//! moves with its model: `light_effort` and a pinned old main effort become
//! `[models].worker_effort`.

use toml::Value as TomlValue;

fn text(table: &toml::value::Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(TomlValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Rewrite a legacy user config table in place. Returns whether it changed.
///
/// The legacy main model's effort belonged to the model that becomes the
/// worker, so it never reaches the new main model: a pinned one (auto effort
/// off) becomes the worker's effort, and the auto fallback is dropped.
pub(crate) fn migrate_model_roles_table(table: &mut toml::value::Table) -> bool {
    let effort_auto = table
        .get("jev")
        .and_then(|jev| jev.get("effort_auto"))
        .and_then(TomlValue::as_bool)
        .unwrap_or(true);
    let legacy_tiers = table
        .get_mut("jev")
        .and_then(TomlValue::as_table_mut)
        .and_then(|jev| jev.remove("tiers"));
    let legacy_light = legacy_tiers
        .as_ref()
        .map(|tiers| tiers.as_table().and_then(|tiers| text(tiers, "light")));
    let legacy_light_effort = legacy_tiers
        .as_ref()
        .and_then(|tiers| tiers.as_table().and_then(|tiers| text(tiers, "light_effort")))
        .filter(|effort| !effort.eq_ignore_ascii_case("auto"));
    let has_reasoning = table
        .get("models")
        .and_then(TomlValue::as_table)
        .is_some_and(|models| models.contains_key("reasoning"));
    if legacy_light.is_none() && !has_reasoning {
        return false;
    }

    let models = table
        .entry("models".to_owned())
        .or_insert_with(|| TomlValue::Table(toml::value::Table::new()));
    if !models.is_table() {
        *models = TomlValue::Table(toml::value::Table::new());
    }
    let models = models.as_table_mut().expect("models is a table");
    if let Some(light) = legacy_light.flatten()
        && !models.contains_key("worker")
    {
        models.insert("worker".to_owned(), TomlValue::String(light));
        if let Some(effort) = legacy_light_effort
            && !models.contains_key("worker_effort")
        {
            models.insert("worker_effort".to_owned(), TomlValue::String(effort));
        }
    }
    let reasoning = models.remove("reasoning");
    let reasoning = reasoning.as_ref().and_then(TomlValue::as_str).map(str::trim);
    let old_main = text(models, "default");
    if let Some(reasoning) = reasoning.filter(|model| !model.is_empty())
        && old_main.as_deref() != Some(reasoning)
    {
        let old_main_effort = models.remove("default_reasoning_effort");
        if !models.contains_key("worker")
            && let Some(old_main) = old_main
        {
            models.insert("worker".to_owned(), TomlValue::String(old_main));
            if !effort_auto
                && let Some(effort) = old_main_effort
                && !models.contains_key("worker_effort")
            {
                models.insert("worker_effort".to_owned(), effort);
            }
        }
        models.insert("default".to_owned(), TomlValue::String(reasoning.to_owned()));
    }
    true
}

/// Migrate `~/.grok/config.toml` once, before anything reads the model roles.
/// A config without the legacy keys is left untouched and is not rewritten.
pub fn migrate_model_roles() -> Result<(), Box<dyn std::error::Error>> {
    crate::config::update_config_toml_locked(&crate::util::distill_home::distill_home(), |table| {
        Ok(migrate_model_roles_table(table))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrate(source: &str) -> (bool, toml::value::Table) {
        let mut table: toml::value::Table = toml::from_str(source).expect("valid toml");
        let changed = migrate_model_roles_table(&mut table);
        (changed, table)
    }

    fn models(table: &toml::value::Table) -> &toml::value::Table {
        table["models"].as_table().expect("[models]")
    }

    /// The strong model the cheaper main model consulted now owns the session
    /// and the cheaper one runs the delegated work. The old main model's effort
    /// was chosen for the model that is now the worker, so the new main model
    /// does not inherit it.
    #[test]
    fn the_reasoning_model_becomes_main_and_the_old_main_the_worker() {
        let (changed, table) = migrate(
            r#"
            [models]
            default = "chatgpt/gpt-6-luna"
            reasoning = "chatgpt/gpt-6-sol"
            default_reasoning_effort = "max"

            [jev]
            effort_auto = true
            "#,
        );
        assert!(changed);
        let models = models(&table);
        assert_eq!(models["default"].as_str(), Some("chatgpt/gpt-6-sol"));
        assert_eq!(models["worker"].as_str(), Some("chatgpt/gpt-6-luna"));
        assert!(!models.contains_key("reasoning"), "the retired key is gone");
        assert!(!models.contains_key("default_reasoning_effort"));
        assert!(
            !models.contains_key("worker_effort"),
            "with auto effort on, `max` was only a fallback: the worker stays on auto"
        );
        assert_eq!(table["jev"]["effort_auto"].as_bool(), Some(true));
    }

    /// A main model pinned to a level (auto effort off) keeps that level when
    /// it becomes the worker; the new main model does not inherit it.
    #[test]
    fn a_pinned_old_main_effort_moves_to_the_worker() {
        let (_, table) = migrate(
            r#"
            [models]
            default = "luna"
            reasoning = "sol"
            default_reasoning_effort = "low"

            [jev]
            effort_auto = false
            "#,
        );
        let models = models(&table);
        assert_eq!(models["default"].as_str(), Some("sol"));
        assert_eq!(models["worker"].as_str(), Some("luna"));
        assert_eq!(models["worker_effort"].as_str(), Some("low"));
        assert!(!models.contains_key("default_reasoning_effort"));
    }

    /// In the oldest layout the default already was the strong model and
    /// `[jev.tiers].light` the model routine steps ran on: that one is the worker.
    #[test]
    fn a_legacy_light_model_becomes_the_worker_and_the_default_stays_main() {
        let (changed, table) = migrate(
            r#"
            [models]
            default = "chatgpt/gpt-6-sol"
            default_reasoning_effort = "high"

            [jev]
            effort_auto = true

            [jev.tiers]
            light = "chatgpt/gpt-6-luna"
            light_effort = "low"
            "#,
        );
        assert!(changed);
        let models = models(&table);
        assert_eq!(models["default"].as_str(), Some("chatgpt/gpt-6-sol"));
        assert_eq!(models["worker"].as_str(), Some("chatgpt/gpt-6-luna"));
        assert_eq!(models["worker_effort"].as_str(), Some("low"), "the fixed light effort moves with it");
        assert_eq!(models["default_reasoning_effort"].as_str(), Some("high"));
        let jev = table["jev"].as_table().expect("[jev] survives");
        assert!(!jev.contains_key("tiers"), "the legacy block is gone");
    }

    /// A cleared reasoning pick, or one equal to the main model, left the main
    /// model working alone: only the retired key goes, nothing is invented.
    #[test]
    fn an_unused_reasoning_key_is_removed_without_changing_the_main_model() {
        for reasoning in ["", "luna"] {
            let (changed, table) = migrate(&format!(
                r#"
                [models]
                default = "luna"
                default_reasoning_effort = "low"
                reasoning = "{reasoning}"
                "#
            ));
            assert!(changed, "{reasoning:?}");
            let models = models(&table);
            assert_eq!(models["default"].as_str(), Some("luna"));
            assert_eq!(models["default_reasoning_effort"].as_str(), Some("low"));
            assert!(!models.contains_key("reasoning") && !models.contains_key("worker"));
        }
    }

    /// A worker the user already picked, including a cleared one, is kept.
    #[test]
    fn an_existing_worker_pick_wins() {
        let (_, table) = migrate(
            r#"
            [models]
            default = "luna"
            reasoning = "sol"
            worker = ""
            "#,
        );
        let models = models(&table);
        assert_eq!(models["default"].as_str(), Some("sol"));
        assert_eq!(models["worker"].as_str(), Some(""));
    }

    /// Migrated configs are not rewritten on every start.
    #[test]
    fn a_config_without_legacy_keys_is_untouched() {
        let source = r#"
            [models]
            default = "sol"
            worker = "luna"
        "#;
        let (changed, table) = migrate(source);
        assert!(!changed);
        assert_eq!(table, toml::from_str::<toml::value::Table>(source).unwrap());
    }
}
