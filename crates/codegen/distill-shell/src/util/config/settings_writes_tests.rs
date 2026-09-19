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
