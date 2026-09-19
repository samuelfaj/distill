//! Display-refresh settings shared by local UI config and remote settings.

use serde::{Deserialize, Serialize};

use crate::deserialize::optional_bool as de_opt_bool_tolerant;

#[cfg(test)]
#[path = "display_refresh_tests.rs"]
mod tests;

/// Display-refresh probe and auto-cadence settings: one struct for local `[ui.display_refresh]`, remote `display_refresh`, and `UiConfig`.
/// Each field deserializes tolerantly (wrong types become `None`); unknown keys land in [`Self::extra`] so a settings save cannot drop future knobs.
/// `resolve_display_refresh` resolves it.
/// Client defaults: probe on, auto on, floor 8 ms, ceiling 16 ms, Hz band 55 to 240.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DisplayRefreshSettings {
    /// Probe the primary display's Hz once per process. `Some(false)` is a kill-switch.
    #[serde(
        default,
        deserialize_with = "de_opt_bool_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub probe_enabled: Option<bool>,
    /// Derive paint/scroll cadence from a successful in-band probe (default off).
    #[serde(
        default,
        deserialize_with = "de_opt_bool_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub auto_cadence_enabled: Option<bool>,
    /// Lower clamp for auto-derived ms (default 8).
    #[serde(
        default,
        deserialize_with = "de_opt_u32_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub floor_ms: Option<u32>,
    /// Upper clamp for auto-derived ms (default 16).
    #[serde(
        default,
        deserialize_with = "de_opt_u32_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub ceiling_ms: Option<u32>,
    /// Minimum accepted probe Hz for auto-cadence (default 55).
    #[serde(
        default,
        deserialize_with = "de_opt_u32_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub min_hz: Option<u32>,
    /// Maximum accepted probe Hz for auto-cadence (default 165).
    #[serde(
        default,
        deserialize_with = "de_opt_u32_tolerant",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_hz: Option<u32>,
    /// Unknown or future object members, preserved across a config rewrite.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl DisplayRefreshSettings {
    /// True when no field is set (all inherit remote/default).
    pub fn is_default(&self) -> bool {
        self.probe_enabled.is_none()
            && self.auto_cadence_enabled.is_none()
            && self.floor_ms.is_none()
            && self.ceiling_ms.is_none()
            && self.min_hz.is_none()
            && self.max_hz.is_none()
            && self.extra.is_empty()
    }
}

fn de_opt_u32_tolerant<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = Option<u32>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("u32 (wrong types ignored)")
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(u32::try_from(v).ok())
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(u32::try_from(v).ok())
        }
        fn visit_u32<E: serde::de::Error>(self, v: u32) -> Result<Self::Value, E> {
            Ok(Some(v))
        }
        fn visit_i32<E: serde::de::Error>(self, v: i32) -> Result<Self::Value, E> {
            Ok(u32::try_from(v).ok())
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<A: serde::de::Deserializer<'de>>(
            self,
            d: A,
        ) -> Result<Self::Value, A::Error> {
            d.deserialize_any(V)
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_string<E: serde::de::Error>(self, _: String) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }
    }
    deserializer.deserialize_any(V)
}
