// Modified for Distill by Samuel Fajreldines, 2026.
//! Persist tests for the `settings_writes` helpers.
//!
//! Each test drives the helper against a throwaway `$GROK_HOME`, then reads the
//! file back the way the consumer of that setting reads it, because a write the
//! consumer cannot see is a silent no-op.

use super::*;
use toml::Value as TomlValue;

/// `[jev.local].model` is the cheap lane's model. The write must land in the live
/// `$GROK_HOME`, leave the rest of `[jev]` (the provider block, the ladder levers
/// the session resolved at startup) exactly as it was, and be visible to the jev
/// resolver — a pick the lane cannot read would do nothing at all.
#[tokio::test]
#[serial_test::serial(GROK_HOME)]
async fn set_jev_local_model_writes_the_pick_and_leaves_the_lane_alone() {
    let home = tempfile::tempdir().expect("home");
    let _guard = distill_test_support::env::EnvGuard::set("GROK_HOME", home.path());
    let path = crate::util::config::user_config_path();
    std::fs::write(
        &path,
        "[jev]\nprovider = \"openrouter_decisions\"\nmodel = \"~typesafe/jev-latest\"\n\n\
         [jev.ladder]\ne_breaker = true\ne_retention = true\n",
    )
    .expect("seed the config the user already has");

    set_jev_local_model("openrouter-qwen37".to_owned())
        .await
        .expect("persist the cheap-lane pick");

    let written: TomlValue = toml::from_str(&std::fs::read_to_string(&path).expect("read back"))
        .expect("the write leaves parseable TOML");
    let jev = written
        .get("jev")
        .and_then(TomlValue::as_table)
        .expect("`[jev]` survives the write");
    assert_eq!(
        jev.get("provider").and_then(TomlValue::as_str),
        Some("openrouter_decisions"),
        "the pick must not cost the provider block"
    );
    assert_eq!(
        jev.get("model").and_then(TomlValue::as_str),
        Some("~typesafe/jev-latest"),
        "the decision model is not this writer's to touch"
    );
    assert_eq!(
        jev.get("ladder")
            .and_then(TomlValue::as_table)
            .and_then(|ladder| ladder.get("e_retention"))
            .and_then(TomlValue::as_bool),
        Some(true),
        "the ladder levers sit in the same section and must survive it"
    );
    assert_eq!(
        jev.get("local")
            .and_then(TomlValue::as_table)
            .and_then(|local| local.get("model"))
            .and_then(TomlValue::as_str),
        Some("openrouter-qwen37"),
        "the cheap-lane pick is the one thing this write changes"
    );

    // The lane resolves `[jev]` from the effective config, not from the raw file.
    let resolved = crate::jev::resolve_config_from_disk();
    assert_eq!(
        resolved.local.model.as_deref(),
        Some("openrouter-qwen37"),
        "a pick the resolver cannot see is a silent no-op"
    );
    assert_eq!(
        resolved.ladder.e_retention,
        Some(true),
        "the levers still resolve on"
    );
}

/// Clearing the pick writes an empty `model`, which the lane reads as "no cheap
/// lane". It cannot delete the key — the merge only ever inserts — so the empty
/// value is the contract, and the resolver must treat it as unset.
#[tokio::test]
#[serial_test::serial(GROK_HOME)]
async fn clearing_the_jev_local_model_reads_back_as_no_cheap_lane() {
    let home = tempfile::tempdir().expect("home");
    let _guard = distill_test_support::env::EnvGuard::set("GROK_HOME", home.path());

    set_jev_local_model("openrouter-qwen37".to_owned())
        .await
        .expect("persist a pick first");
    set_jev_local_model(String::new())
        .await
        .expect("clear the pick");

    let written: TomlValue = toml::from_str(
        &std::fs::read_to_string(crate::util::config::user_config_path()).expect("read back"),
    )
    .expect("parse");
    assert_eq!(
        written
            .get("jev")
            .and_then(|jev| jev.get("local"))
            .and_then(|local| local.get("model"))
            .and_then(TomlValue::as_str),
        Some(""),
        "the key stays, holding nothing"
    );
    assert_eq!(
        crate::jev::resolve_config_from_disk()
            .local
            .model
            .as_deref(),
        Some(""),
        "the empty pick is what the lane has to read as unset"
    );
}

/// Both tiers take `auto` or a level, and each write must land where its reader
/// looks: the worker's in `[models].worker_effort` (read by the subagent
/// resolver), the main model's as `[jev] effort_auto` plus
/// `[models].default_reasoning_effort` (read when a session starts). Switching
/// the main model back to auto keeps the last level as its fallback.
#[tokio::test]
#[serial_test::serial(GROK_HOME)]
async fn tier_efforts_persist_auto_or_a_level_where_their_readers_look() {
    use distill_sampling_types::ReasoningEffort;
    let home = tempfile::tempdir().expect("home");
    let _guard = distill_test_support::env::EnvGuard::set("GROK_HOME", home.path());
    let path = crate::util::config::user_config_path();
    std::fs::write(
        &path,
        "[models]\ndefault = \"sol\"\nworker = \"luna\"\n\n[jev]\nprovider = \"openrouter_decisions\"\n",
    )
    .expect("seed the config");
    let read = || -> TomlValue {
        toml::from_str(&std::fs::read_to_string(&path).expect("read back")).expect("valid TOML")
    };

    set_worker_effort(Some(ReasoningEffort::Medium)).await.expect("pin the worker");
    assert_eq!(read()["models"]["worker_effort"].as_str(), Some("medium"));
    set_worker_effort(None).await.expect("worker back to auto");
    assert_eq!(read()["models"]["worker_effort"].as_str(), Some("auto"));

    set_main_effort(Some(ReasoningEffort::High)).await.expect("pin the main model");
    let pinned = read();
    assert_eq!(pinned["jev"]["effort_auto"].as_bool(), Some(false));
    assert_eq!(pinned["models"]["default_reasoning_effort"].as_str(), Some("high"));
    assert_eq!(
        pinned["jev"]["provider"].as_str(),
        Some("openrouter_decisions"),
        "the provider block survives"
    );
    set_main_effort(None).await.expect("main back to auto");
    let auto = read();
    assert_eq!(auto["jev"]["effort_auto"].as_bool(), Some(true));
    assert_eq!(
        auto["models"]["default_reasoning_effort"].as_str(),
        Some("high"),
        "the last level stays as the fallback"
    );
    assert_eq!(auto["models"]["worker"].as_str(), Some("luna"), "the tiers are untouched");
}

/// "Don't ask again" on the cold-return compaction offer writes `[compaction] cold_return =
/// "off"`. The section also holds the pruning and memory-flush settings the session resolved at
/// startup, which must survive, and the offer reads the mode from the effective config: a write
/// it cannot see would keep asking after the user said not to.
#[tokio::test]
#[serial_test::serial(GROK_HOME)]
async fn cold_return_off_lands_where_the_offer_reads_it() {
    use crate::session::acp_session::cold_return::ColdReturnMode;
    let home = tempfile::tempdir().expect("home");
    let _guard = distill_test_support::env::EnvGuard::set("GROK_HOME", home.path());
    let path = crate::util::config::user_config_path();
    std::fs::write(&path, "[compaction.pruning]\nkeep_last_n_turns = 7\n")
        .expect("seed the config the user already has");

    set_compaction_cold_return("off".to_owned())
        .await
        .expect("persist the answer");

    let written: TomlValue = toml::from_str(&std::fs::read_to_string(&path).expect("read back"))
        .expect("the write leaves parseable TOML");
    assert_eq!(written["compaction"]["cold_return"].as_str(), Some("off"));
    assert_eq!(
        written["compaction"]["pruning"]["keep_last_n_turns"].as_integer(),
        Some(7),
        "the pruning settings share the section and must survive"
    );
    let effective = crate::config::load_effective_config().expect("effective config");
    assert_eq!(
        ColdReturnMode::from_config(&effective),
        ColdReturnMode::Off,
        "an answer the offer cannot read is a silent no-op"
    );
}

#[test]
fn utility_model_warning_only_targets_non_catalog_lanes() {
    assert_eq!(
        utility_model_warning("chatgpt/gpt-6-luna", true, false),
        None
    );
    assert_eq!(utility_model_warning("vendor/model", false, true), None);
    assert_eq!(
        utility_model_warning("vendor/model", false, false).as_deref(),
        Some(
            "Utility model `vendor/model` cannot build a lane with current credentials; utility work will be skipped."
        )
    );
    assert_eq!(utility_model_warning("", false, false), None);
}
