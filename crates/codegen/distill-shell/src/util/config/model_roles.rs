//! One-shot migration to the main/reasoning model layout.
//!
//! Earlier builds kept the model that owned every session in `[jev.tiers].light`
//! (the "worker") and repurposed `[models].default` as the reasoning model.
//! Now `[models].default` is the **main** model (`/model`, required) and
//! `[models].reasoning` the optional **reasoning** model (`/reasoning-model`).

use toml::Value as TomlValue;

/// Rewrite a legacy user config table in place. Returns whether it changed.
///
/// The legacy worker becomes the main model and the legacy default becomes the
/// reasoning model. A concrete worker effort becomes the main model's effort;
/// the legacy default's effort belonged to the model that is now the reasoning
/// model, so it is dropped rather than applied to the main one.
pub(crate) fn migrate_model_roles_table(table: &mut toml::value::Table) -> bool {
    let Some(tiers) = table
        .get_mut("jev")
        .and_then(TomlValue::as_table_mut)
        .and_then(|jev| jev.remove("tiers"))
    else {
        return false;
    };
    let tiers = tiers.as_table().cloned().unwrap_or_default();
    let text = |key: &str| {
        tiers
            .get(key)
            .and_then(TomlValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let Some(worker) = text("light") else {
        return true;
    };
    let worker_effort = text("light_effort").filter(|effort| !effort.eq_ignore_ascii_case("auto"));

    let models = table
        .entry("models".to_owned())
        .or_insert_with(|| TomlValue::Table(toml::value::Table::new()));
    if !models.is_table() {
        *models = TomlValue::Table(toml::value::Table::new());
    }
    let models = models.as_table_mut().expect("models is a table");
    let legacy_default = models
        .get("default")
        .and_then(TomlValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    if !models.contains_key("reasoning")
        && let Some(reasoning) = legacy_default.filter(|model| *model != worker)
    {
        models.insert("reasoning".to_owned(), TomlValue::String(reasoning));
    }
    models.insert("default".to_owned(), TomlValue::String(worker));
    match worker_effort {
        Some(effort) => {
            models.insert(
                "default_reasoning_effort".to_owned(),
                TomlValue::String(effort),
            );
        }
        None => {
            models.remove("default_reasoning_effort");
        }
    }
    true
}

/// Migrate `~/.grok/config.toml` once, before anything reads the model roles.
/// A config without `[jev.tiers]` is left untouched and is not rewritten.
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

    /// The model the user picked with `/model` ran every session as the worker,
    /// so it must stay in charge as the main model; the old default was the
    /// reasoning model and keeps that role.
    #[test]
    fn the_worker_becomes_main_and_the_old_default_becomes_reasoning() {
        let (changed, table) = migrate(
            r#"
            [models]
            default = "chatgpt/gpt-6-sol"
            default_reasoning_effort = "high"

            [jev]
            effort_auto = true

            [jev.tiers]
            light = "chatgpt/gpt-6-luna"
            light_effort = "auto"
            "#,
        );
        assert!(changed);
        let models = models(&table);
        assert_eq!(models["default"].as_str(), Some("chatgpt/gpt-6-luna"));
        assert_eq!(models["reasoning"].as_str(), Some("chatgpt/gpt-6-sol"));
        assert!(
            !models.contains_key("default_reasoning_effort"),
            "`high` was the reasoning model's effort; the main model keeps auto"
        );
        let jev = table["jev"].as_table().expect("[jev] survives");
        assert!(!jev.contains_key("tiers"), "the legacy block is gone");
        assert_eq!(jev["effort_auto"].as_bool(), Some(true));
    }

    /// A fixed worker effort was the user's choice for the model that is now
    /// main, so it moves with it.
    #[test]
    fn a_fixed_worker_effort_moves_to_the_main_model() {
        let (_, table) = migrate(
            r#"
            [models]
            default = "sol"
            [jev.tiers]
            light = "luna"
            light_effort = "low"
            "#,
        );
        assert_eq!(
            models(&table)["default_reasoning_effort"].as_str(),
            Some("low")
        );
    }

    /// Without a worker, the old default already owned every session: it is
    /// the main model, and no reasoning model is invented.
    #[test]
    fn without_a_worker_the_default_stays_main_and_no_reasoning_is_set() {
        let (changed, table) = migrate(
            r#"
            [models]
            default = "sol"
            default_reasoning_effort = "high"
            [jev.tiers]
            light = ""
            "#,
        );
        assert!(changed, "the empty legacy block is removed");
        let models = models(&table);
        assert_eq!(models["default"].as_str(), Some("sol"));
        assert_eq!(models["default_reasoning_effort"].as_str(), Some("high"));
        assert!(!models.contains_key("reasoning"));
    }

    /// The same model in both roles would make the main model consult itself.
    #[test]
    fn a_worker_equal_to_the_default_sets_no_reasoning_model() {
        let (_, table) = migrate(
            r#"
            [models]
            default = "luna"
            [jev.tiers]
            light = "luna"
            "#,
        );
        assert!(!models(&table).contains_key("reasoning"));
    }

    /// An explicit reasoning pick (including a cleared one) is never replaced.
    #[test]
    fn an_existing_reasoning_pick_wins() {
        let (_, table) = migrate(
            r#"
            [models]
            default = "sol"
            reasoning = ""
            [jev.tiers]
            light = "luna"
            "#,
        );
        assert_eq!(models(&table)["reasoning"].as_str(), Some(""));
    }

    /// Migrated configs are not rewritten on every start.
    #[test]
    fn a_config_without_legacy_tiers_is_untouched() {
        let source = r#"
            [models]
            default = "luna"
            reasoning = "sol"
        "#;
        let (changed, table) = migrate(source);
        assert!(!changed);
        assert_eq!(table, toml::from_str::<toml::value::Table>(source).unwrap());
    }
}
