use super::DisplayRefreshSettings;

#[test]
fn json_round_trip_preserves_partial_settings_and_unknown_keys() {
    let settings: DisplayRefreshSettings = serde_json::from_value(serde_json::json!({
        "probe_enabled": false,
        "auto_cadence_enabled": "invalid",
        "floor_ms": 7,
        "ceiling_ms": -1,
        "min_hz": 4_294_967_296_u64,
        "max_hz": 144,
        "future_knob": {"enabled": true},
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(settings).unwrap(),
        serde_json::json!({
            "probe_enabled": false,
            "floor_ms": 7,
            "max_hz": 144,
            "future_knob": {"enabled": true},
        })
    );
}

#[test]
fn toml_round_trip_preserves_partial_settings_and_unknown_keys() {
    let settings: DisplayRefreshSettings = toml::from_str(
        "probe_enabled = false\nauto_cadence_enabled = 'invalid'\nfloor_ms = 7\n\
         ceiling_ms = -1\nmin_hz = 4294967296\nmax_hz = 144\nfuture_knob = 'kept'\n",
    )
    .unwrap();
    assert!(!settings.is_default());
    let serialized = toml::to_string(&settings).unwrap();
    assert_eq!(
        toml::from_str::<toml::Value>(&serialized).unwrap(),
        toml::from_str::<toml::Value>(
            "probe_enabled = false\nfloor_ms = 7\nmax_hz = 144\nfuture_knob = 'kept'\n"
        )
        .unwrap()
    );
    assert_eq!(
        toml::from_str::<DisplayRefreshSettings>(&serialized).unwrap(),
        settings
    );
}

#[test]
fn tolerant_boolean_consumes_invalid_collections_without_losing_siblings() {
    for invalid in [
        serde_json::json!([true, false]),
        serde_json::json!({"nested": true}),
    ] {
        let settings: DisplayRefreshSettings = serde_json::from_value(serde_json::json!({
            "probe_enabled": invalid,
            "auto_cadence_enabled": true,
            "floor_ms": 8,
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(settings).unwrap(),
            serde_json::json!({"auto_cadence_enabled": true, "floor_ms": 8})
        );
    }
}

#[test]
fn empty_settings_round_trip_as_empty_object() {
    let settings: DisplayRefreshSettings = serde_json::from_str("{}").unwrap();
    assert!(settings.is_default());
    assert_eq!(serde_json::to_string(&settings).unwrap(), "{}");
}
