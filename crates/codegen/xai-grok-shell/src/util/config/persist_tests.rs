use super::super::load::load_config_from_toml;
use super::super::mcp::{
    JevLocalPersistConfig, JevPersistConfig, JevTiersPersistConfig, McpConfig,
    parse_mcp_config_with_oauth,
};
use super::*;
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;
/// First-run `ensure` creates a 0-byte `$GROK_HOME/config.toml`.
/// Empty and whitespace-only files must parse as an empty table so the first settings write is not "refusing to overwrite unparseable".
/// Non-empty garbage still refuses.
#[test]
fn parse_existing_config_toml_blank_is_empty_table_not_unparseable() {
    for blank in ["", "   ", "\n", "\n\t  \n", " \r\n "] {
        let v = parse_existing_config_toml(blank)
            .unwrap_or_else(|e| panic!("blank {blank:?} must be an empty table, got {e}"));
        assert_eq!(
            v,
            TomlValue::Table(TomlMap::new()),
            "blank {blank:?} must be an empty table"
        );
    }
    let parsed = parse_existing_config_toml("[ui]\nyolo = true\n").unwrap();
    assert_eq!(
        parsed
            .get("ui")
            .and_then(|u| u.get("yolo"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        parse_existing_config_toml("this is [not valid toml\n").is_err(),
        "non-empty unparseable TOML must still be refused"
    );
}
/// The `[toolset.ask_user_question]` settings write merges only that sub-table.
/// The toggled field lands, hand-written sibling keys survive, and no other `[toolset]` defaults (bash/web_search) are splatted into the user file.
/// All-None leaves the file untouched.
#[test]
fn ask_user_question_merge_writes_subtable_without_splatting_toolset() {
    let root_val: TomlValue =
        toml::from_str("[toolset.ask_user_question]\ntimeout_secs = 30\n").unwrap();
    let mut root = root_val.as_table().unwrap().clone();
    let ask = crate::tools::config::AskUserQuestionToolConfig {
        timeout_enabled: Some(false),
        ..Default::default()
    };
    merge_ask_user_question_section(&mut root, &ask);
    let toolset = root.get("toolset").and_then(|v| v.as_table()).unwrap();
    assert_eq!(toolset.len(), 1, "only ask_user_question may be written");
    let ask_tbl = toolset
        .get("ask_user_question")
        .and_then(|v| v.as_table())
        .unwrap();
    assert_eq!(
        ask_tbl.get("timeout_enabled").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        ask_tbl.get("timeout_secs").and_then(|v| v.as_integer()),
        Some(30),
        "hand-written sibling keys must survive the merge"
    );
    let reparsed = load_config_from_toml(&TomlValue::Table(root.clone()));
    assert_eq!(reparsed.ask_user_question.timeout_enabled, Some(false));
    assert_eq!(reparsed.ask_user_question.timeout_secs, Some(30));
    let mut empty_root: TomlMap<String, TomlValue> = TomlMap::new();
    merge_ask_user_question_section(
        &mut empty_root,
        &crate::tools::config::AskUserQuestionToolConfig::default(),
    );
    assert!(
        empty_root.is_empty(),
        "all-None must not create an empty [toolset] header"
    );
    let mut scalar_root: TomlMap<String, TomlValue> = TomlMap::new();
    scalar_root.insert("toolset".into(), TomlValue::String("bogus".into()));
    merge_ask_user_question_section(&mut scalar_root, &ask);
    assert_eq!(
        scalar_root
            .get("toolset")
            .and_then(|v| v.get("ask_user_question"))
            .and_then(|a| a.get("timeout_enabled"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "scalar [toolset] must be replaced so the write lands"
    );
}
/// The `[telemetry]` write merges only `trace_upload`: hand-written sibling telemetry keys survive, and an all-None config leaves the section alone.
#[test]
fn telemetry_merge_writes_trace_upload_without_splatting_section() {
    let root_val: TomlValue =
        toml::from_str("[telemetry]\ncustom_telemetry_flag = true\n").unwrap();
    let mut root = root_val.as_table().unwrap().clone();
    let telemetry = super::super::mcp::TelemetryPersistConfig {
        trace_upload: Some(true),
    };
    merge_section(&mut root, "telemetry", &telemetry);
    let section = root.get("telemetry").and_then(|v| v.as_table()).unwrap();
    assert_eq!(
        section.get("trace_upload").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        section
            .get("custom_telemetry_flag")
            .and_then(|v| v.as_bool()),
        Some(true),
        "hand-written sibling keys must survive the merge"
    );
    let reparsed = load_config_from_toml(&TomlValue::Table(root.clone()));
    assert_eq!(reparsed.telemetry.trace_upload, Some(true));
    let untouched_val: TomlValue = toml::from_str("[telemetry]\nevents_url = \"x\"\n").unwrap();
    let mut untouched = untouched_val.as_table().unwrap().clone();
    merge_section(
        &mut untouched,
        "telemetry",
        &super::super::mcp::TelemetryPersistConfig::default(),
    );
    assert_eq!(
        untouched
            .get("telemetry")
            .and_then(|v| v.get("events_url"))
            .and_then(|v| v.as_str()),
        Some("x"),
        "all-None must leave the existing section untouched"
    );
}
/// The `[features]` write merges only `feedback_trace_card`.
/// Hand-written sibling feature keys survive, and an all-None config leaves the section alone.
#[test]
fn features_merge_writes_feedback_trace_card_without_splatting_section() {
    let root_val: TomlValue = toml::from_str("[features]\nweb_fetch = true\n").unwrap();
    let mut root = root_val.as_table().unwrap().clone();
    let features = super::super::mcp::FeaturesPersistConfig {
        feedback_trace_card: Some(false),
    };
    merge_section(&mut root, "features", &features);
    let section = root.get("features").and_then(|v| v.as_table()).unwrap();
    assert_eq!(
        section.get("feedback_trace_card").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        section.get("web_fetch").and_then(|v| v.as_bool()),
        Some(true),
        "hand-written sibling keys must survive the merge"
    );
    let reparsed = load_config_from_toml(&TomlValue::Table(root.clone()));
    assert_eq!(reparsed.features.feedback_trace_card, Some(false));
    let untouched_val: TomlValue = toml::from_str("[features]\nvoice_mode = false\n").unwrap();
    let mut untouched = untouched_val.as_table().unwrap().clone();
    merge_section(
        &mut untouched,
        "features",
        &super::super::mcp::FeaturesPersistConfig::default(),
    );
    assert_eq!(
        untouched
            .get("features")
            .and_then(|v| v.get("voice_mode"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "all-None must leave the existing section untouched"
    );
}
#[test]
fn transport_oauth_client_id_takes_priority_over_block() {
    let json = r#"{
            "mcpServers": {
                "svc": {
                    "type": "http",
                    "url": "https://svc.example/mcp",
                    "oauth_client_id": "transport-client",
                    "oauth": { "clientId": "block-client" }
                }
            }
        }"#;
    let config: McpConfig = serde_json::from_str(json).expect("parse .mcp.json");
    let svc = config.mcp_servers.get("svc").unwrap();
    let oauth = svc.oauth_config().expect("oauth_config");
    assert_eq!(oauth.client_id.as_deref(), Some("transport-client"));
}
#[test]
fn parse_mcp_config_with_oauth_extracts_byo_client_id() {
    let json = r#"{
            "mcpServers": {
                "slack": {
                    "type": "http",
                    "url": "https://mcp.slack.example/mcp",
                    "oauth": { "clientId": "slack-byo-client" }
                },
                "plain": {
                    "type": "http",
                    "url": "https://plain.example/mcp"
                }
            }
        }"#;
    let config: McpConfig = serde_json::from_str(json).expect("parse .mcp.json");
    let (servers, oauth) = parse_mcp_config_with_oauth(&config, "test", &|s| s.to_string());
    assert_eq!(servers.len(), 2);
    assert_eq!(oauth.len(), 1);
    assert_eq!(
        oauth.get("slack").unwrap().client_id.as_deref(),
        Some("slack-byo-client")
    );
    assert!(!oauth.contains_key("plain"));
}
/// The merge recurses into nested tables and only ever inserts.
/// A key inside `[ui.status_line]` that this build does not model is therefore not at risk from a settings write.
/// The status-line parser relies on this: it reports an unknown key rather than refusing to persist the section over it.
#[test]
fn merge_section_preserves_unmodeled_fields_inside_a_nested_table() {
    let mut table = TomlMap::new();
    let mut status_line = TomlMap::new();
    status_line.insert("type".into(), TomlValue::String("builtin".into()));
    status_line.insert("colour".into(), TomlValue::String("red".into()));
    let mut ui = TomlMap::new();
    ui.insert("status_line".into(), TomlValue::Table(status_line));
    table.insert("ui".into(), TomlValue::Table(ui));
    let cfg = crate::agent::config::UiConfig {
        status_line: xai_grok_status_line::test_support::StatusLineConfigFixture::from_kind(
            xai_grok_status_line::StatusLineType::Command,
        )
        .with_command("~/status_line.sh")
        .into_config(),
        ..Default::default()
    };
    merge_section(&mut table, "ui", &cfg);
    let written = table
        .get("ui")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("status_line"))
        .and_then(|v| v.as_table())
        .expect("the section survives");
    assert_eq!(
        written.get("colour").and_then(|v| v.as_str()),
        Some("red"),
        "a key this build does not model must survive a write of the ones it does"
    );
    assert_eq!(
        written.get("type").and_then(|v| v.as_str()),
        Some("command")
    );
}
#[test]
fn merge_section_preserves_unmodeled_fields() {
    let mut table = TomlMap::new();
    let mut ui = TomlMap::new();
    ui.insert("show_timestamps".into(), TomlValue::Boolean(true));
    ui.insert(
        "auto_dark_theme".into(),
        TomlValue::String("tokyonight".into()),
    );
    ui.insert("custom_user_key".into(), TomlValue::Integer(42));
    table.insert("ui".into(), TomlValue::Table(ui));
    let cfg = crate::agent::config::UiConfig::default();
    merge_section(&mut table, "ui", &cfg);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("show_timestamps").and_then(|v| v.as_bool()),
        Some(true),
        "pre-existing show_timestamps should survive merge with default struct"
    );
    assert_eq!(
        ui.get("auto_dark_theme").and_then(|v| v.as_str()),
        Some("tokyonight"),
        "pre-existing auto_dark_theme should survive merge with default struct"
    );
    assert_eq!(
        ui.get("custom_user_key").and_then(|v| v.as_integer()),
        Some(42),
        "truly unmodeled user-added key should survive merge"
    );
}
#[test]
fn merge_section_nested_display_refresh_preserves_future_knob() {
    let mut table = TomlMap::new();
    let mut ui = TomlMap::new();
    let mut dr = TomlMap::new();
    dr.insert("probe_enabled".into(), TomlValue::Boolean(true));
    dr.insert("future_knob".into(), TomlValue::Integer(42));
    ui.insert("display_refresh".into(), TomlValue::Table(dr));
    table.insert("ui".into(), TomlValue::Table(ui));
    let mut cfg = crate::agent::config::UiConfig::default();
    cfg.display_refresh.probe_enabled = Some(false);
    merge_section(&mut table, "ui", &cfg);
    let nested = table
        .get("ui")
        .and_then(|v| v.as_table())
        .and_then(|u| u.get("display_refresh"))
        .and_then(|v| v.as_table())
        .expect("display_refresh table");
    assert_eq!(
        nested.get("probe_enabled").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        nested.get("future_knob").and_then(|v| v.as_integer()),
        Some(42),
        "unknown nested keys must survive shallow-looking settings writes"
    );
}
#[test]
fn merge_section_updates_modeled_fields_preserving_unmodeled() {
    let mut table = TomlMap::new();
    let mut ui = TomlMap::new();
    ui.insert("yolo".into(), TomlValue::Boolean(false));
    ui.insert("show_timestamps".into(), TomlValue::Boolean(true));
    ui.insert(
        "auto_light_theme".into(),
        TomlValue::String("grokday".into()),
    );
    table.insert("ui".into(), TomlValue::Table(ui));
    let cfg = crate::agent::config::UiConfig {
        yolo: true,
        ..Default::default()
    };
    merge_section(&mut table, "ui", &cfg);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("yolo").and_then(|v| v.as_bool()),
        Some(true),
        "modeled field yolo should be updated"
    );
    assert_eq!(
        ui.get("show_timestamps").and_then(|v| v.as_bool()),
        Some(true),
        "pre-existing field not in serialized output should be preserved"
    );
    assert_eq!(
        ui.get("auto_light_theme").and_then(|v| v.as_str()),
        Some("grokday"),
        "pre-existing field not in serialized output should be preserved"
    );
}
#[test]
fn merge_section_creates_new_section() {
    let mut table = TomlMap::new();
    assert!(table.get("ui").is_none());
    let cfg = crate::agent::config::UiConfig {
        yolo: true,
        ..Default::default()
    };
    merge_section(&mut table, "ui", &cfg);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(ui.get("yolo").and_then(|v| v.as_bool()), Some(true));
}
/// A default `SessionConfig` save must not write `load_envrc`; a stray `true` would override a managed-config `load_envrc = false`.
/// `load_envrc` is `Option<bool>` with `skip_serializing_if`, so a field the user never set stays off disk.
#[test]
fn merge_section_session_default_does_not_leak_load_envrc() {
    let mut table = TomlMap::new();
    assert!(table.get("session").is_none());
    let cfg = crate::agent::config::SessionConfig::default();
    merge_section(&mut table, "session", &cfg);
    if let Some(session) = table.get("session").and_then(|v| v.as_table()) {
        assert!(
            session.get("load_envrc").is_none(),
            "PR 12 R1 Bug 1: pager-side save with default SessionConfig must \
             NOT serialize load_envrc — that would override managed-config \
             policy. Found: {:?}",
            session.get("load_envrc"),
        );
        assert!(
            session.get("auto_compact_threshold_percent").is_none(),
            "default auto_compact_threshold_percent must not be serialized either"
        );
    }
}
/// Companion to the above: the user explicitly commits a non-default `auto_compact_threshold_percent`.
/// The field is serialized but `load_envrc` (still default None) is NOT.
/// Pins the asymmetry: committing one [session] field does not "claim" the rest.
#[test]
fn merge_section_session_explicit_value_does_not_drag_load_envrc() {
    let mut table = TomlMap::new();
    let mut session = TomlMap::new();
    session.insert("load_envrc".into(), TomlValue::Boolean(false));
    table.insert("session".into(), TomlValue::Table(session));
    let cfg = crate::agent::config::SessionConfig {
        auto_compact_threshold_percent: Some(70),
        load_envrc: None,
    };
    merge_section(&mut table, "session", &cfg);
    let session = table.get("session").unwrap().as_table().unwrap();
    assert_eq!(
        session
            .get("auto_compact_threshold_percent")
            .and_then(|v| v.as_integer()),
        Some(70),
    );
    assert_eq!(
        session.get("load_envrc").and_then(|v| v.as_bool()),
        Some(false),
        "pre-existing load_envrc must survive a partial settings save"
    );
}
/// Follow-on: the user DOES explicitly set `load_envrc = false` via TOML. The value round-trips correctly through `load_config_from_toml`, a mutation, and `merge_section`.
/// `None` means "absent on disk"; `Some(false)` means "user explicitly disabled". The distinction must survive a save.
#[test]
fn session_load_envrc_explicit_false_round_trips() {
    let raw_config: TomlValue = toml::from_str(
        r#"
            [session]
            load_envrc = false
            "#,
    )
    .unwrap();
    let cfg = load_config_from_toml(&raw_config);
    assert_eq!(
        cfg.session.load_envrc,
        Some(false),
        "explicit load_envrc = false on disk must load as Some(false), not None"
    );
    let mut table = TomlMap::new();
    merge_section(&mut table, "session", &cfg.session);
    let session = table.get("session").unwrap().as_table().unwrap();
    assert_eq!(
        session.get("load_envrc").and_then(|v| v.as_bool()),
        Some(false),
        "explicit load_envrc = false must survive a save"
    );
}
#[test]
fn merge_section_empty_struct_preserves_existing_section() {
    let mut table = TomlMap::new();
    let mut harness = TomlMap::new();
    harness.insert("custom_key".into(), TomlValue::Boolean(true));
    harness.insert("another_key".into(), TomlValue::String("value".into()));
    table.insert("harness".into(), TomlValue::Table(harness));
    let cfg = crate::agent::config::HarnessConfig::default();
    merge_section(&mut table, "harness", &cfg);
    let harness = table.get("harness").unwrap().as_table().unwrap();
    assert_eq!(
        harness.get("custom_key").and_then(|v| v.as_bool()),
        Some(true),
        "existing fields must survive when struct serializes empty"
    );
    assert_eq!(
        harness.get("another_key").and_then(|v| v.as_str()),
        Some("value"),
    );
}
#[test]
fn ui_config_round_trip_preserves_pager_fields() {
    let toml_str = r#"
[ui]
yolo = true
show_timestamps = false
auto_dark_theme = "tokyonight"
auto_light_theme = "grokday"
"#;
    let root: TomlValue = toml::from_str(toml_str).unwrap();
    let cfg = load_config_from_toml(&root);
    assert!(cfg.ui.yolo);
    assert_eq!(cfg.ui.show_timestamps, Some(false));
    assert_eq!(cfg.ui.auto_dark_theme.as_deref(), Some("tokyonight"));
    assert_eq!(cfg.ui.auto_light_theme.as_deref(), Some("grokday"));
    let mut table = root.as_table().unwrap().clone();
    merge_section(&mut table, "ui", &cfg.ui);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("show_timestamps").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        ui.get("auto_dark_theme").and_then(|v| v.as_str()),
        Some("tokyonight")
    );
    assert_eq!(
        ui.get("auto_light_theme").and_then(|v| v.as_str()),
        Some("grokday")
    );
    assert_eq!(ui.get("yolo").and_then(|v| v.as_bool()), Some(true));
}
#[test]
fn ui_config_hunk_tracker_mode_round_trips() {
    let root: TomlValue = toml::from_str("[ui]\nhunk_tracker_mode = \"off\"\n").unwrap();
    let cfg = load_config_from_toml(&root);
    assert_eq!(cfg.ui.hunk_tracker_mode.as_deref(), Some("off"));
    let mut table = root.as_table().unwrap().clone();
    merge_section(&mut table, "ui", &cfg.ui);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("hunk_tracker_mode").and_then(|v| v.as_str()),
        Some("off")
    );
    let serialized = TomlValue::try_from(crate::agent::config::UiConfig::default()).unwrap();
    assert!(
        serialized
            .as_table()
            .unwrap()
            .get("hunk_tracker_mode")
            .is_none(),
        "hunk_tracker_mode=None must not appear in serialized output"
    );
}
#[test]
fn ui_config_serialization_behavior() {
    let cfg = crate::agent::config::UiConfig::default();
    let val = TomlValue::try_from(&cfg).unwrap();
    let table = val.as_table().unwrap();
    assert!(
        table.get("yolo").is_some(),
        "yolo must always serialize so revert-to-default persists"
    );
    assert!(
        table.get("compact_mode").is_some(),
        "compact_mode must always serialize so revert-to-default persists"
    );
    assert!(
        table.get("max_thoughts_width").is_some(),
        "max_thoughts_width must always serialize so revert-to-default persists"
    );
    assert!(
        table.get("show_timestamps").is_none(),
        "show_timestamps=None should not appear in serialized output"
    );
    assert!(
        table.get("auto_dark_theme").is_none(),
        "auto_dark_theme=None should not appear in serialized output"
    );
    assert!(
        table.get("theme").is_none(),
        "theme=None should not appear in serialized output"
    );
}
/// The settings-modal helpers in the parent module are 3-line wrappers around `update_config(|cfg| cfg.ui.<field> = ...)`.
/// To guard against future drift between the wrapper and the schema field, this test simulates each helper's closure against an in-memory `Config`. We deliberately avoid disk I/O so the test stays hermetic.
/// The pattern mirrors exactly what `update_config` does internally: `let mut cfg = load_config_from_toml(...); f(&mut cfg);`.
#[test]
fn merge_section_full_save_config_simulation() {
    let original = r#"
[ui]
show_timestamps = true
auto_dark_theme = "tokyonight"
auto_light_theme = "grokday"

[models]
default = "grok-3"

[cli]
auto_update = true
"#;
    let root: TomlValue = toml::from_str(original).unwrap();
    let mut cfg = load_config_from_toml(&root);
    cfg.models.default = Some("grok-4".to_string());
    let mut table = root.as_table().unwrap().clone();
    merge_section(&mut table, "cli", &cfg.cli);
    merge_section(&mut table, "models", &cfg.models);
    merge_section(&mut table, "ui", &cfg.ui);
    merge_section(&mut table, "harness", &cfg.harness);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("show_timestamps").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        ui.get("auto_dark_theme").and_then(|v| v.as_str()),
        Some("tokyonight")
    );
    assert_eq!(
        ui.get("auto_light_theme").and_then(|v| v.as_str()),
        Some("grokday")
    );
    let models = table.get("models").unwrap().as_table().unwrap();
    assert_eq!(
        models.get("default").and_then(|v| v.as_str()),
        Some("grok-4")
    );
}
#[test]
fn merge_section_revert_to_default_overwrites_old_value() {
    let mut table = TomlMap::new();
    let mut ui = TomlMap::new();
    ui.insert("yolo".into(), TomlValue::Boolean(true));
    ui.insert("compact_mode".into(), TomlValue::Boolean(true));
    table.insert("ui".into(), TomlValue::Table(ui));
    let cfg = crate::agent::config::UiConfig::default();
    merge_section(&mut table, "ui", &cfg);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("yolo").and_then(|v| v.as_bool()),
        Some(false),
        "yolo=false must overwrite the old yolo=true"
    );
    assert_eq!(
        ui.get("compact_mode").and_then(|v| v.as_bool()),
        Some(false),
        "compact_mode=false must overwrite the old compact_mode=true"
    );
}
#[test]
fn merge_section_replaces_non_table_section() {
    let mut table = TomlMap::new();
    table.insert("ui".into(), TomlValue::String("garbage".into()));
    let cfg = crate::agent::config::UiConfig {
        yolo: true,
        ..Default::default()
    };
    merge_section(&mut table, "ui", &cfg);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("yolo").and_then(|v| v.as_bool()),
        Some(true),
        "non-table section should be replaced with proper table"
    );
}
#[test]
fn models_config_serializes_only_some_fields() {
    let m = crate::agent::config::ModelsConfig {
        default: Some("grok-3".to_string()),
        ..Default::default()
    };
    let v = TomlValue::try_from(&m).expect("serialize ModelsConfig");
    if let TomlValue::Table(t) = v {
        assert_eq!(t.len(), 1);
        assert!(t.contains_key("default"));
        assert!(!t.contains_key("web_search"));
        assert!(!t.contains_key("session_summary"));
        assert!(!t.contains_key("image_description"));
        assert!(!t.contains_key("hidden_models"));
        assert!(!t.contains_key("disabled_models"));
        assert!(!t.contains_key("allowed_models"));
        assert!(!t.contains_key("agent_type"));
        assert_eq!(t.get("default").and_then(|x| x.as_str()), Some("grok-3"));
    } else {
        panic!("expected table from serialization");
    }
}
/// Canonical list of every `Option<T>` field in [`CliConfig`].
/// Kept in one place so both serialization and merge-section tests automatically cover newly-added fields without copy-pasting assertion lists.
const CLI_CONFIG_OPTION_FIELDS: &[&str] = &[
    "auto_update",
    "dismissed_version",
    "installer",
    "npm_registry",
    "channel",
    "use_leader",
    "show_tips",
    "worktree_type",
    "session_registry",
    "minimum_version",
    "maximum_version",
    "required_minimum_version",
    "required_maximum_version",
];
/// Assert that every `CliConfig` `Option<T>` field NOT in `present` is absent from `table`.
fn assert_cli_option_fields_absent(table: &TomlMap<String, TomlValue>, present: &[&str]) {
    for field in CLI_CONFIG_OPTION_FIELDS {
        if !present.contains(field) {
            assert!(
                !table.contains_key(*field),
                "expected Option field `{field}` to be absent when not set",
            );
        }
    }
}
#[test]
fn cli_config_serializes_only_some_fields() {
    let c = crate::agent::config::CliConfig {
        auto_update: Some(true),
        channel: Some("beta".to_string()),
        ..Default::default()
    };
    let v = TomlValue::try_from(&c).expect("serialize CliConfig");
    if let TomlValue::Table(t) = v {
        assert_eq!(t.len(), 2);
        assert!(t.contains_key("auto_update"));
        assert!(t.contains_key("channel"));
        assert_cli_option_fields_absent(&t, &["auto_update", "channel"]);
        assert_eq!(t.get("auto_update").and_then(|x| x.as_bool()), Some(true));
        assert_eq!(t.get("channel").and_then(|x| x.as_str()), Some("beta"));
    } else {
        panic!("expected table from serialization");
    }
}
#[test]
fn merge_section_cli_only_updates_set_fields_preserves_unmodeled() {
    let mut table = TomlMap::new();
    let mut cli = TomlMap::new();
    cli.insert("use_leader".into(), TomlValue::Boolean(true));
    cli.insert("show_tips".into(), TomlValue::Boolean(false));
    cli.insert(
        "custom_pager_key".into(),
        TomlValue::String("keep-this".into()),
    );
    table.insert("cli".into(), TomlValue::Table(cli));
    let cfg = crate::agent::config::CliConfig {
        auto_update: Some(false),
        dismissed_version: Some("v1.2.3".to_string()),
        ..Default::default()
    };
    merge_section(&mut table, "cli", &cfg);
    let c = table.get("cli").unwrap().as_table().unwrap();
    assert_eq!(c.get("auto_update").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(
        c.get("dismissed_version").and_then(|v| v.as_str()),
        Some("v1.2.3")
    );
    assert_eq!(c.get("use_leader").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(c.get("show_tips").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(
        c.get("custom_pager_key").and_then(|v| v.as_str()),
        Some("keep-this")
    );
    assert_cli_option_fields_absent(
        c,
        &[
            "auto_update",
            "dismissed_version",
            "use_leader",
            "show_tips",
        ],
    );
}
#[test]
fn merge_section_models_only_updates_set_fields_preserves_others() {
    let mut table = TomlMap::new();
    let mut models = TomlMap::new();
    models.insert("web_search".into(), TomlValue::String("old-search".into()));
    models.insert("unmodeled_foo".into(), TomlValue::String("keep-me".into()));
    table.insert("models".into(), TomlValue::Table(models));
    let cfg = crate::agent::config::ModelsConfig {
        default: Some("grok-new".to_string()),
        ..Default::default()
    };
    merge_section(&mut table, "models", &cfg);
    let m = table.get("models").unwrap().as_table().unwrap();
    assert_eq!(m.get("default").and_then(|v| v.as_str()), Some("grok-new"));
    assert_eq!(
        m.get("web_search").and_then(|v| v.as_str()),
        Some("old-search")
    );
    assert_eq!(
        m.get("unmodeled_foo").and_then(|v| v.as_str()),
        Some("keep-me")
    );
    assert!(!m.contains_key("session_summary"));
}
#[test]
fn persist_preferred_model_flow_roundtrips_via_load_and_new_from_toml_cfg() {
    let original = "[models]\ndefault = \"grok-old\"\nweb_search = \"some-search\"\n";
    let root: TomlValue = toml::from_str(original).unwrap();
    let mut cfg = load_config_from_toml(&root);
    cfg.models.default = Some("grok-persisted".to_string());
    let mut table = if let TomlValue::Table(t) = root {
        t
    } else {
        TomlMap::new()
    };
    merge_section(&mut table, "models", &cfg.models);
    let reloaded_root = TomlValue::Table(table);
    let reloaded = load_config_from_toml(&reloaded_root);
    assert_eq!(reloaded.models.default.as_deref(), Some("grok-persisted"));
    let cfg2 =
        crate::agent::config::Config::new_from_toml_cfg(&reloaded_root).expect("new_from_toml_cfg");
    assert_eq!(cfg2.models.default.as_deref(), Some("grok-persisted"));
}
#[test]
fn merge_section_cli_show_tips_writes_under_cli_section() {
    let mut table = TomlMap::new();
    let cfg = crate::agent::config::CliConfig {
        show_tips: Some(false),
        ..Default::default()
    };
    merge_section(&mut table, "cli", &cfg);
    let c = table.get("cli").unwrap().as_table().unwrap();
    assert_eq!(
        c.get("show_tips").and_then(|v| v.as_bool()),
        Some(false),
        "set_show_tips must persist Some(false) at `[cli].show_tips`"
    );
}
#[test]
fn merge_section_cli_show_tips_none_does_not_serialize() {
    let mut table = TomlMap::new();
    let cfg = crate::agent::config::CliConfig::default();
    assert!(cfg.show_tips.is_none());
    merge_section(&mut table, "cli", &cfg);
    if let Some(c) = table.get("cli").and_then(|v| v.as_table()) {
        assert!(
            c.get("show_tips").is_none(),
            "default show_tips: None must not serialize — \
             managed-config layering depends on absent-means-defer"
        );
    }
}
#[test]
fn merge_section_cli_session_picker_grouped_writes_under_cli_section() {
    let mut table = TomlMap::new();
    let cfg = crate::agent::config::CliConfig {
        session_picker_grouped: Some(false),
        ..Default::default()
    };
    merge_section(&mut table, "cli", &cfg);
    let c = table.get("cli").unwrap().as_table().unwrap();
    assert_eq!(
        c.get("session_picker_grouped").and_then(|v| v.as_bool()),
        Some(false),
        "Some(false) must round-trip to `[cli].session_picker_grouped`"
    );
}
#[test]
fn merge_section_cli_auto_update_writes_under_cli_section() {
    let mut table = TomlMap::new();
    let cfg = crate::agent::config::CliConfig {
        auto_update: Some(false),
        ..Default::default()
    };
    merge_section(&mut table, "cli", &cfg);
    let c = table.get("cli").unwrap().as_table().unwrap();
    assert_eq!(
        c.get("auto_update").and_then(|v| v.as_bool()),
        Some(false),
        "set_auto_update must persist Some(false) at `[cli].auto_update`"
    );
}
#[test]
fn merge_section_cli_use_leader_writes_under_cli_section() {
    let mut table = TomlMap::new();
    let cfg = crate::agent::config::CliConfig {
        use_leader: Some(true),
        ..Default::default()
    };
    merge_section(&mut table, "cli", &cfg);
    let c = table.get("cli").unwrap().as_table().unwrap();
    assert_eq!(
        c.get("use_leader").and_then(|v| v.as_bool()),
        Some(true),
        "Some(true) must round-trip to `[cli].use_leader`"
    );
}
/// Verify that `Option<bool>` with `skip_serializing_if` prevents one `[session]` field from dragging unrelated fields.
#[test]
fn merge_section_session_load_envrc_writes_under_session_section() {
    let mut table = TomlMap::new();
    let cfg = crate::agent::config::SessionConfig {
        load_envrc: Some(false),
        ..Default::default()
    };
    merge_section(&mut table, "session", &cfg);
    let s = table.get("session").unwrap().as_table().unwrap();
    assert_eq!(
        s.get("load_envrc").and_then(|v| v.as_bool()),
        Some(false),
        "Some(false) must round-trip to `[session].load_envrc`"
    );
}
#[test]
fn merge_section_session_load_envrc_does_not_drag_auto_compact() {
    let mut table = TomlMap::new();
    let cfg = crate::agent::config::SessionConfig {
        load_envrc: Some(true),
        auto_compact_threshold_percent: None,
    };
    merge_section(&mut table, "session", &cfg);
    let s = table.get("session").unwrap().as_table().unwrap();
    assert_eq!(s.get("load_envrc").and_then(|v| v.as_bool()), Some(true));
    assert!(
        s.get("auto_compact_threshold_percent").is_none(),
        "default auto_compact_threshold_percent: None must not serialize \
         when only load_envrc is being committed"
    );
}
mod resolve_auto_compact {
    use super::super::super::RemoteSettings;
    use super::super::super::resolve::{
        DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT, ENV_AUTO_COMPACT_THRESHOLD_PERCENT,
        resolve_auto_compact_threshold_percent,
    };
    use crate::agent::config::{Config, ConfigModelOverride, ModelInfo};
    use std::sync::Mutex;
    const TEST_MODEL: &str = "grok-4.5";
    const OTHER_MODEL: &str = "grok-4.3";
    /// Serialize tests that mutate `GROK_AUTO_COMPACT_THRESHOLD_PERCENT`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    /// Build a `Config` populated with optional per-source values for the `TEST_MODEL`.
    /// Any `None` argument means "that source is unset".
    fn make_cfg(
        user_session: Option<u8>,
        user_per_model: Option<u8>,
        gb_global: Option<u8>,
    ) -> Config {
        let mut cfg = Config::default();
        cfg.session.auto_compact_threshold_percent = user_session;
        if let Some(v) = user_per_model {
            cfg.config_models.insert(
                TEST_MODEL.to_owned(),
                ConfigModelOverride {
                    auto_compact_threshold_percent: Some(v),
                    ..ConfigModelOverride::default()
                },
            );
        }
        if let Some(v) = gb_global {
            cfg.remote_settings = Some(RemoteSettings {
                auto_compact_threshold_percent: Some(v),
                ..RemoteSettings::default()
            });
        }
        cfg
    }
    /// ModelInfo populated with the GB per-model value (or none).
    fn model_info(gb_per_model: Option<u8>) -> ModelInfo {
        let mut info = ModelInfo::fallback(TEST_MODEL);
        info.auto_compact_threshold_percent = gb_per_model;
        info
    }
    fn resolve(cfg: &Config, gb_per_model: Option<u8>) -> u8 {
        let info = model_info(gb_per_model);
        resolve_auto_compact_threshold_percent(cfg, TEST_MODEL, Some(&info))
    }
    /// RAII guard that swaps the env var for the duration of a test and restores the previous value on drop.
    /// Acquires `ENV_LOCK` so two env-var tests never run concurrently.
    struct EnvVarGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<String>,
    }
    impl EnvVarGuard {
        fn set(value: &str) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT).ok();
            unsafe { std::env::set_var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT, value) };
            Self { _lock: lock, prev }
        }
        fn unset() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT).ok();
            unsafe { std::env::remove_var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT) };
            Self { _lock: lock, prev }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => unsafe { std::env::set_var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT, v) },
                None => unsafe { std::env::remove_var(ENV_AUTO_COMPACT_THRESHOLD_PERCENT) },
            }
        }
    }
    #[test]
    fn all_unset_returns_default_85() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(None, None, None);
        assert_eq!(resolve(&cfg, None), DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
    }
    #[test]
    fn all_unset_no_model_info_returns_default_85() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(None, None, None);
        assert_eq!(
            resolve_auto_compact_threshold_percent(&cfg, TEST_MODEL, None),
            DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT
        );
    }
    #[test]
    fn gb_global_only() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(None, None, Some(40));
        assert_eq!(resolve(&cfg, None), 40);
    }
    #[test]
    fn gb_per_model_beats_gb_global() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(None, None, Some(40));
        assert_eq!(resolve(&cfg, Some(90)), 90);
    }
    #[test]
    fn user_session_beats_gb_per_model() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(Some(75), None, None);
        assert_eq!(resolve(&cfg, Some(90)), 75);
    }
    #[test]
    fn user_session_beats_gb_global() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(Some(75), None, Some(40));
        assert_eq!(resolve(&cfg, None), 75);
    }
    #[test]
    fn user_per_model_beats_user_session() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(Some(75), Some(70), None);
        assert_eq!(resolve(&cfg, None), 70);
    }
    #[test]
    fn user_per_model_beats_gb_per_model() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(None, Some(70), None);
        assert_eq!(resolve(&cfg, Some(90)), 70);
    }
    #[test]
    fn user_per_model_beats_gb_global() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(None, Some(70), Some(40));
        assert_eq!(resolve(&cfg, None), 70);
    }
    #[test]
    fn user_per_model_beats_everything_below_env() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(Some(75), Some(70), Some(40));
        assert_eq!(resolve(&cfg, Some(90)), 70);
    }
    #[test]
    fn env_beats_user_per_model() {
        let _g = EnvVarGuard::set("50");
        let cfg = make_cfg(Some(75), Some(70), Some(40));
        assert_eq!(resolve(&cfg, Some(90)), 50);
    }
    #[test]
    fn env_at_lower_bound_is_honored() {
        let _g = EnvVarGuard::set("0");
        let cfg = make_cfg(Some(75), None, None);
        assert_eq!(resolve(&cfg, None), 0);
    }
    #[test]
    fn env_at_upper_bound_is_honored() {
        let _g = EnvVarGuard::set("100");
        let cfg = make_cfg(Some(75), None, None);
        assert_eq!(resolve(&cfg, None), 100);
    }
    #[test]
    fn env_out_of_range_high_falls_through() {
        let _g = EnvVarGuard::set("101");
        let cfg = make_cfg(Some(75), None, None);
        assert_eq!(resolve(&cfg, None), 75);
    }
    #[test]
    fn env_out_of_range_negative_falls_through() {
        let _g = EnvVarGuard::set("-1");
        let cfg = make_cfg(Some(75), None, None);
        assert_eq!(resolve(&cfg, None), 75);
    }
    #[test]
    fn env_unparseable_falls_through() {
        let _g = EnvVarGuard::set("not-a-number");
        let cfg = make_cfg(Some(75), None, None);
        assert_eq!(resolve(&cfg, None), 75);
    }
    #[test]
    fn env_empty_falls_through_to_default() {
        let _g = EnvVarGuard::set("");
        let cfg = make_cfg(None, None, None);
        assert_eq!(resolve(&cfg, None), DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
    }
    #[test]
    fn user_per_model_for_other_model_does_not_match() {
        let _g = EnvVarGuard::unset();
        let mut cfg = Config::default();
        cfg.session.auto_compact_threshold_percent = None;
        cfg.config_models.insert(
            OTHER_MODEL.to_owned(),
            ConfigModelOverride {
                auto_compact_threshold_percent: Some(70),
                ..ConfigModelOverride::default()
            },
        );
        assert_eq!(resolve(&cfg, None), DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
    }
    #[test]
    fn user_per_model_for_other_model_falls_through_to_user_session() {
        let _g = EnvVarGuard::unset();
        let mut cfg = Config::default();
        cfg.session.auto_compact_threshold_percent = Some(75);
        cfg.config_models.insert(
            OTHER_MODEL.to_owned(),
            ConfigModelOverride {
                auto_compact_threshold_percent: Some(70),
                ..ConfigModelOverride::default()
            },
        );
        assert_eq!(resolve(&cfg, None), 75);
    }
    #[test]
    fn missing_model_info_falls_through_to_gb_global() {
        let _g = EnvVarGuard::unset();
        let cfg = make_cfg(None, None, Some(40));
        assert_eq!(
            resolve_auto_compact_threshold_percent(&cfg, TEST_MODEL, None),
            40
        );
    }
    #[test]
    fn no_remote_settings_falls_through_to_default() {
        let _g = EnvVarGuard::unset();
        let cfg = Config {
            remote_settings: None,
            ..Config::default()
        };
        assert_eq!(resolve(&cfg, None), DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT);
    }
    #[test]
    fn apply_does_not_merge_auto_compact_threshold_percent_into_model_info() {
        use crate::agent::config::{EndpointsConfig, ModelEntry};
        let endpoints = EndpointsConfig::default();
        let base = ModelEntry::fallback(TEST_MODEL, &endpoints);
        let over = ConfigModelOverride {
            auto_compact_threshold_percent: Some(42),
            ..ConfigModelOverride::default()
        };
        let merged = over.apply(TEST_MODEL, Some(base), &endpoints);
        assert_eq!(
            merged.info.auto_compact_threshold_percent, None,
            "ConfigModelOverride::apply must NOT merge `auto_compact_threshold_percent` \
             into ModelInfo — the resolver depends on the field staying empty so \
             user-per-model and GB-per-model remain distinguishable tiers"
        );
    }
}
#[test]
fn settings_helpers_target_correct_ui_fields() {
    fn apply<F: FnOnce(&mut Config)>(f: F) -> Config {
        let mut cfg = load_config_from_toml(&TomlValue::Table(TomlMap::new()));
        f(&mut cfg);
        cfg
    }
    let cfg = apply(|cfg| cfg.ui.compact_mode = true);
    assert!(cfg.ui.compact_mode, "set_compact_mode must set bool field");
    let cfg = apply(|cfg| cfg.ui.compact_mode = false);
    assert!(!cfg.ui.compact_mode);
    let cfg = apply(|cfg| cfg.ui.show_timestamps = Some(true));
    assert_eq!(cfg.ui.show_timestamps, Some(true));
    let cfg = apply(|cfg| cfg.ui.show_timestamps = Some(false));
    assert_eq!(cfg.ui.show_timestamps, Some(false));
    let cfg = apply(|cfg| cfg.ui.simple_mode = Some(true));
    assert_eq!(cfg.ui.simple_mode, Some(true));
    let cfg = apply(|cfg| cfg.ui.simple_mode = Some(false));
    assert_eq!(cfg.ui.simple_mode, Some(false));
    let cfg = apply(|cfg| cfg.ui.theme = Some("tokyonight".to_string()));
    assert_eq!(cfg.ui.theme, Some("tokyonight".to_string()));
    let cfg = apply(|cfg| cfg.ui.theme = Some("auto".to_string()));
    assert_eq!(cfg.ui.theme, Some("auto".to_string()));
    let cfg = apply(|cfg| cfg.ui.auto_dark_theme = Some("tokyonight".to_string()));
    assert_eq!(cfg.ui.auto_dark_theme, Some("tokyonight".to_string()));
    let cfg = apply(|cfg| cfg.ui.auto_light_theme = Some("grokday".to_string()));
    assert_eq!(cfg.ui.auto_light_theme, Some("grokday".to_string()));
    let cfg = apply(|cfg| cfg.ui.hunk_tracker_mode = Some("off".to_string()));
    assert_eq!(cfg.ui.hunk_tracker_mode, Some("off".to_string()));
    let cfg = apply(|cfg| cfg.ui.screen_mode = Some("minimal".to_string()));
    assert_eq!(cfg.ui.screen_mode, Some("minimal".to_string()));
    let cfg = apply(|cfg| cfg.ui.screen_mode = Some("fullscreen".to_string()));
    assert_eq!(cfg.ui.screen_mode, Some("fullscreen".to_string()));
}
#[test]
fn set_theme_round_trips_through_merge() {
    let original = r#"
[ui]
compact_mode = true
theme = "groknight"
auto_dark_theme = "tokyonight"
custom_user_key = "preserve-me"
"#;
    let root: TomlValue = toml::from_str(original).unwrap();
    let mut cfg = load_config_from_toml(&root);
    cfg.ui.theme = Some("tokyonight".to_string());
    let mut table = root.as_table().unwrap().clone();
    merge_section(&mut table, "ui", &cfg.ui);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("theme").and_then(|v| v.as_str()),
        Some("tokyonight"),
        "theme must be set"
    );
    assert_eq!(
        ui.get("compact_mode").and_then(|v| v.as_bool()),
        Some(true),
        "unrelated modeled field must survive"
    );
    assert_eq!(
        ui.get("auto_dark_theme").and_then(|v| v.as_str()),
        Some("tokyonight"),
        "auto_dark_theme must survive"
    );
    assert_eq!(
        ui.get("custom_user_key").and_then(|v| v.as_str()),
        Some("preserve-me"),
        "unmodeled field must survive"
    );
}
#[test]
fn set_auto_dark_and_light_theme_round_trip_through_merge() {
    let original = r#"
[ui]
theme = "auto"
auto_dark_theme = "groknight"
auto_light_theme = "grokday"
custom_unknown_key = 42
"#;
    let root: TomlValue = toml::from_str(original).unwrap();
    let mut cfg = load_config_from_toml(&root);
    cfg.ui.auto_dark_theme = Some("tokyonight".to_string());
    cfg.ui.auto_light_theme = Some("rosepine-moon".to_string());
    let mut table = root.as_table().unwrap().clone();
    merge_section(&mut table, "ui", &cfg.ui);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("auto_dark_theme").and_then(|v| v.as_str()),
        Some("tokyonight"),
    );
    assert_eq!(
        ui.get("auto_light_theme").and_then(|v| v.as_str()),
        Some("rosepine-moon"),
    );
    assert_eq!(
        ui.get("theme").and_then(|v| v.as_str()),
        Some("auto"),
        "theme=auto must survive"
    );
    assert_eq!(
        ui.get("custom_unknown_key").and_then(|v| v.as_integer()),
        Some(42),
        "unmodeled field must survive"
    );
}
#[test]
fn set_compact_mode_round_trips_through_merge() {
    let original = r#"
[ui]
compact_mode = false
auto_dark_theme = "tokyonight"
show_timestamps = true
custom_user_key = "preserve-me"
"#;
    let root: TomlValue = toml::from_str(original).unwrap();
    let mut cfg = load_config_from_toml(&root);
    cfg.ui.compact_mode = true;
    let mut table = root.as_table().unwrap().clone();
    merge_section(&mut table, "ui", &cfg.ui);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("compact_mode").and_then(|v| v.as_bool()),
        Some(true),
        "compact_mode must be flipped"
    );
    assert_eq!(
        ui.get("auto_dark_theme").and_then(|v| v.as_str()),
        Some("tokyonight"),
        "unrelated UI field must survive"
    );
    assert_eq!(
        ui.get("show_timestamps").and_then(|v| v.as_bool()),
        Some(true),
        "show_timestamps must survive"
    );
    assert_eq!(
        ui.get("custom_user_key").and_then(|v| v.as_str()),
        Some("preserve-me"),
        "unmodeled (unknown to the UiConfig schema) field must survive — \
         this is the merge_section invariant the new helpers depend on"
    );
}
#[test]
fn set_show_timestamps_and_simple_mode_round_trip_through_merge() {
    let original = r#"
[ui]
compact_mode = true
custom_unknown_key = 42
"#;
    let root: TomlValue = toml::from_str(original).unwrap();
    let mut cfg = load_config_from_toml(&root);
    cfg.ui.show_timestamps = Some(false);
    cfg.ui.simple_mode = Some(false);
    let mut table = root.as_table().unwrap().clone();
    merge_section(&mut table, "ui", &cfg.ui);
    let ui = table.get("ui").unwrap().as_table().unwrap();
    assert_eq!(
        ui.get("show_timestamps").and_then(|v| v.as_bool()),
        Some(false),
    );
    assert_eq!(ui.get("simple_mode").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(
        ui.get("compact_mode").and_then(|v| v.as_bool()),
        Some(true),
        "unrelated modeled field must survive"
    );
    assert_eq!(
        ui.get("custom_unknown_key").and_then(|v| v.as_integer()),
        Some(42),
        "unmodeled (unknown to the schema) field must survive"
    );
}
#[cfg(unix)]
fn project_slot_symlink(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let outside = dir.join("outside.toml");
    std::fs::write(&outside, "keep\n").unwrap();
    let link = dir.join(".grok").join("config.toml");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    (link, outside)
}
#[cfg(unix)]
fn dotfiles_config_symlink(
    dir: &std::path::Path,
    contents: Option<&str>,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let repo = dir.join("dotfiles");
    std::fs::create_dir_all(&repo).unwrap();
    let target = repo.join("config.toml");
    if let Some(contents) = contents {
        std::fs::write(&target, contents).unwrap();
    }
    let link = dir.join("config.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    (link, target)
}
#[cfg(unix)]
fn assert_still_symlink(path: &std::path::Path) {
    assert!(
        std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
/// Project `.grok/config.toml` must replace a leaf symlink, not follow it.
#[cfg(unix)]
#[test]
fn atomic_replace_string_replaces_project_config_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let (link, outside) = project_slot_symlink(dir.path());
    atomic_replace_string(&link, "[mcp_servers]\n").unwrap();
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        !meta.file_type().is_symlink(),
        "project config symlink must be replaced: {:?}",
        meta.file_type()
    );
    assert_eq!("[mcp_servers]\n", std::fs::read_to_string(&link).unwrap());
    assert_eq!("keep\n", std::fs::read_to_string(&outside).unwrap());
}
/// No user grok home: persist must resolve the cwd `.grok/config.toml` as a
/// slot (replace), not follow an external referent.
#[cfg(unix)]
#[test]
fn no_home_cwd_config_resolves_slot_not_follow() {
    let dir = tempfile::tempdir().unwrap();
    let (link, outside) = project_slot_symlink(dir.path());
    let followed = bind_user_config_dest_with(&link, true, true).unwrap();
    let slot = bind_user_config_dest_with(&link, true, false).unwrap();
    assert_eq!(
        outside,
        followed.as_path(),
        "with a user home, follow the referent"
    );
    assert_eq!(
        link,
        slot.as_path(),
        "without a user home, replace the slot inode"
    );
    xai_grok_config::fs_atomic::require_same_follow_destination(&link, slot.as_path())
        .expect_err("follow check rejects a slot bind");
    let dest = require_same_user_config_dest_with(&link, &slot, false).unwrap();
    atomic_write_resolved_string(&dest, "new\n").unwrap();
    assert_eq!("new\n", std::fs::read_to_string(&link).unwrap());
    assert_eq!("keep\n", std::fs::read_to_string(&outside).unwrap());
}
/// A 0600 referent must not be published via a 0644 temp (chmod-ignored).
#[cfg(unix)]
#[test]
fn atomic_write_string_preserves_0600_referent_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "old\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    atomic_write_string(&path, "new\n").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(0o600, mode, "must not publish 0644 over a 0600 referent");
    assert_eq!("new\n", std::fs::read_to_string(&path).unwrap());
}
/// A retarget after bind refuses the write.
#[cfg(unix)]
#[test]
fn follow_bound_rmw_refuses_retarget_between_read_and_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.toml");
    let b = dir.path().join("b.toml");
    std::fs::write(&a, "from_a = true\n").unwrap();
    std::fs::write(&b, "from_b = true\n").unwrap();
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&a, &link).unwrap();
    let (dest, content) = read_follow_bound(&link).unwrap();
    assert_eq!("from_a = true\n", content);
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&b, &link).unwrap();
    let err = atomic_write_follow_bound(&link, &dest, "from_a = true\nmerged = true\n")
        .expect_err("retarget");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert_eq!("from_a = true\n", std::fs::read_to_string(&a).unwrap());
    assert_eq!("from_b = true\n", std::fs::read_to_string(&b).unwrap());
}
/// Same-path inode replace after bind must refuse.
#[cfg(unix)]
#[test]
fn follow_bound_rmw_refuses_same_path_inode_replace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest_file = dir.path().join("a.toml");
    std::fs::write(&dest_file, "from_a = true\n").unwrap();
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&dest_file, &link).unwrap();
    let (dest, content) = read_follow_bound(&link).unwrap();
    assert_eq!("from_a = true\n", content);
    std::fs::remove_file(&dest_file).unwrap();
    std::fs::write(&dest_file, "from_b = true\n").unwrap();
    let err = atomic_write_follow_bound(&link, &dest, "from_a = true\nmerged = true\n")
        .expect_err("inode replace");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert_eq!(
        "from_b = true\n",
        std::fs::read_to_string(&dest_file).unwrap()
    );
}
/// Bind missing dest, then swap the parent directory for a symlink to B.
#[cfg(unix)]
#[test]
fn follow_bound_rmw_refuses_absent_dest_ancestor_retarget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("A");
    let b = dir.path().join("B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let dest_path = a.join("config.toml");
    let bound = bind_user_config_dest_with(&dest_path, true, true).unwrap();
    std::fs::remove_dir(&a).unwrap();
    std::os::unix::fs::symlink(&b, &a).unwrap();
    let err = atomic_write_resolved_string(&bound, "from_a\n").expect_err("ancestor");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert!(!b.join("config.toml").exists(), "must not write B");
}
/// `config.toml` as a symlink into a repo file: tmp+rename must write the referent.
#[cfg(unix)]
#[test]
fn atomic_write_string_preserves_config_toml_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (link, target) = dotfiles_config_symlink(dir.path(), Some("[ui]\nsimple_mode = false\n"));
    atomic_write_string(&link, "[ui]\nsimple_mode = true\n").unwrap();
    assert_still_symlink(&link);
    assert_eq!(target, std::fs::read_link(&link).unwrap());
    assert_eq!(
        "[ui]\nsimple_mode = true\n",
        std::fs::read_to_string(&target).unwrap()
    );
}
#[cfg(unix)]
#[test]
fn atomic_write_string_creates_dangling_symlink_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (link, target) = dotfiles_config_symlink(dir.path(), None);
    atomic_write_string(&link, "created\n").unwrap();
    assert_still_symlink(&link);
    assert_eq!("created\n", std::fs::read_to_string(&target).unwrap());
}
/// Trailing `/` on a dangling target must not create a regular sibling that later
/// reads through the symlink see as `ENOTDIR`.
#[cfg(unix)]
#[test]
fn atomic_write_string_refuses_trailing_slash_dangling_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing-target");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink("missing-target/", &link).unwrap();
    let err = atomic_write_string(&link, "created\n").expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert!(!missing.exists(), "must not create a regular sibling");
    assert_still_symlink(&link);
}
#[cfg(unix)]
#[test]
fn atomic_write_string_refuses_trailing_dot_dangling_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing-target");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink("missing-target/.", &link).unwrap();
    let err = atomic_write_string(&link, "created\n").expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert!(!missing.exists(), "must not create a regular sibling");
}
#[cfg(unix)]
#[test]
fn atomic_write_string_follows_relative_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("dotfiles");
    std::fs::create_dir_all(&repo).unwrap();
    let target = repo.join("config.toml");
    std::fs::write(&target, "before\n").unwrap();
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink("dotfiles/config.toml", &link).unwrap();
    atomic_write_string(&link, "after\n").unwrap();
    assert_still_symlink(&link);
    assert_eq!("after\n", std::fs::read_to_string(&target).unwrap());
}
/// `file/../config.toml` must not resolve to the sibling (kernel `ENOTDIR`).
#[cfg(unix)]
#[test]
fn atomic_write_string_refuses_parent_through_regular_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    let victim = dir.path().join("config.toml");
    std::fs::write(&file, "file").unwrap();
    std::fs::write(&victim, "keep\n").unwrap();
    let err = atomic_write_string(&file.join("..").join("config.toml"), "clobber\n")
        .expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert_eq!("keep\n", std::fs::read_to_string(&victim).unwrap());
}
/// `missing/../config.toml` must not create `missing` then rename over the symlink.
#[cfg(unix)]
#[test]
fn atomic_write_string_refuses_parent_through_missing_then_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing");
    let target = dir.path().join("dotfiles.toml");
    let link = dir.path().join("config.toml");
    std::fs::write(&target, "keep\n").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let err = atomic_write_string(&missing.join("..").join("config.toml"), "clobber\n")
        .expect_err("ENOENT");
    assert_eq!(std::io::ErrorKind::NotFound, err.kind());
    assert!(!missing.exists(), "must not create_dir_all the missing hop");
    assert_still_symlink(&link);
    assert_eq!("keep\n", std::fs::read_to_string(&target).unwrap());
}
/// Directory squat at the destination: fail closed, do not replace with a file.
#[cfg(unix)]
#[test]
fn atomic_write_string_refuses_existing_directory_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let squat = dir.path().join("config.toml");
    std::fs::create_dir(&squat).unwrap();
    let err = atomic_write_string(&squat, "clobber\n").expect_err("EISDIR");
    assert_eq!(std::io::ErrorKind::IsADirectory, err.kind());
    assert!(squat.is_dir(), "directory squat must remain a directory");
}
/// Trailing `/` or `/.` on the destination path itself (not a symlink target).
#[cfg(unix)]
#[test]
fn atomic_write_string_refuses_trailing_slash_on_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let victim = dir.path().join("config.toml");
    std::fs::write(&victim, "keep\n").unwrap();
    let slash = {
        let mut p = victim.clone().into_os_string();
        p.push("/");
        std::path::PathBuf::from(p)
    };
    let err = atomic_write_string(&slash, "clobber\n").expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert_eq!("keep\n", std::fs::read_to_string(&victim).unwrap());
    let dot = {
        let mut p = victim.into_os_string();
        p.push("/.");
        std::path::PathBuf::from(p)
    };
    let err = atomic_write_string(&dot, "clobber\n").expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
}
/// On Unix `\` is a valid filename character; save through `target\` must work.
#[cfg(unix)]
#[test]
fn atomic_write_string_follows_unix_backslash_filename_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("target\\");
    std::fs::write(&target, "before\n").unwrap();
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    atomic_write_string(&link, "after\n").unwrap();
    assert_still_symlink(&link);
    assert_eq!("after\n", std::fs::read_to_string(&target).unwrap());
}
/// Cancelling the `run_blocking` await must not drop the write guard before the worker finishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_blocking_save_holds_write_guard_until_worker_finishes() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    let guard = tokio::time::timeout(Duration::from_secs(5), lock_config_writes())
        .await
        .expect("first lock timed out")
        .expect("first lock");
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let started_w = started.clone();
    let finished_w = finished.clone();
    let task = tokio::spawn(async move {
        guard
            .run_blocking(move || {
                started_w.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(250));
                finished_w.store(true, Ordering::SeqCst);
                Ok::<(), std::io::Error>(())
            })
            .await
    });
    let wait_started = tokio::time::timeout(Duration::from_secs(2), async {
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(wait_started.is_ok(), "blocking worker never started");
    task.abort();
    let _ = task.await;
    let _g2 = tokio::time::timeout(Duration::from_secs(2), lock_config_writes())
        .await
        .expect("second lock timed out")
        .expect("second lock");
    assert!(
        finished.load(Ordering::SeqCst),
        "second writer acquired locks before the detached save released them"
    );
}

/// Picking a cheap-lane model must not cost the jev lane its other settings.
/// The write models only `[jev.local].model`, and `[jev]` is where the provider
/// block and the `[jev.ladder]` levers live, so a merge that dropped them would
/// silently switch lanes off the moment the user chose a model.
#[test]
fn merging_the_jev_slice_writes_the_cheap_model_and_preserves_the_lane() {
    let mut table = TomlMap::new();
    let mut ladder = TomlMap::new();
    ladder.insert("e_breaker".into(), TomlValue::Boolean(true));
    ladder.insert("e_retention".into(), TomlValue::Boolean(true));
    let mut jev = TomlMap::new();
    jev.insert(
        "provider".into(),
        TomlValue::String("openrouter_decisions".into()),
    );
    jev.insert("ladder".into(), TomlValue::Table(ladder));
    table.insert("jev".into(), TomlValue::Table(jev));

    merge_section(
        &mut table,
        "jev",
        &JevPersistConfig {
            local: Some(JevLocalPersistConfig {
                model: Some("openrouter-qwen37".into()),
            }),
            tiers: Some(JevTiersPersistConfig {
                light: Some("codex-luna".into()),
            }),
        },
    );

    let jev = table
        .get("jev")
        .and_then(|v| v.as_table())
        .expect("the `[jev]` table survives the write");
    assert_eq!(
        jev.get("provider").and_then(|v| v.as_str()),
        Some("openrouter_decisions"),
        "the provider block is not this writer's to touch"
    );
    assert_eq!(
        jev.get("ladder")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("e_breaker"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "the ladder levers sit beside the pick and must survive it"
    );
    assert_eq!(
        jev.get("local")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("model"))
            .and_then(|v| v.as_str()),
        Some("openrouter-qwen37"),
        "the cheap-lane pick is the one thing this write changes"
    );
    assert_eq!(
        jev.get("tiers")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("light"))
            .and_then(|v| v.as_str()),
        Some("codex-luna"),
        "the light sibling rides in the same `[jev]` table and survives too"
    );
}
