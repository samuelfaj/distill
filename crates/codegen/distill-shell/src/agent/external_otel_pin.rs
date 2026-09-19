// Modified for Distill by Samuel Fajreldines, 2026.
//! Requirements pin + destination lock for the external OTEL stream.
//!
//! Every present `[telemetry] otel_*` key in merged `requirements.toml` is a
//! pin (managed-env model). `managed_config.toml` is not a lock.
//! Collector tokens never live in TOML — keys containing `header` are ignored.
//!
//! [`resolve_external_otel_config_with`] overlays pins onto an injected getenv
//! and **must not** mutate process env. The production path
//! ([`apply_process_env_strip`]) `remove_var`s conflicting developer `OTEL_*`
//! so children cannot inherit a decoy endpoint. The same strip matrix also
//! hides unlisted user/managed file siblings — env-only hide would let
//! `otel_logs_endpoint` in `config.toml` retarget a pinned generic endpoint.

use std::collections::{HashMap, HashSet};

const CONTENT_GATE_KEYS: &[&str] = &[
    "otel_log_user_prompts",
    "otel_log_tool_details",
    "otel_log_assistant_responses",
    "otel_log_tool_content",
];

const GENERIC_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
const LOGS_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT";
const METRICS_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT";
#[cfg(test)]
const ENDPOINT_ENV: &[&str] = &[GENERIC_ENDPOINT, LOGS_ENDPOINT, METRICS_ENDPOINT];

const GENERIC_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
const LOGS_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL";
const METRICS_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL";
#[cfg(test)]
const PROTOCOL_ENV: &[&str] = &[GENERIC_PROTOCOL, LOGS_PROTOCOL, METRICS_PROTOCOL];

/// Generic + per-signal siblings: pin the generic key → strip unlisted
/// logs/metrics env names so a developer per-signal decoy cannot inherit.
struct GenericSignalFamily {
    generic_file: &'static str,
    generic_aliases: &'static [&'static str],
    generic_env: &'static str,
    logs_file: &'static str,
    logs_env: &'static str,
    metrics_file: &'static str,
    metrics_env: &'static str,
}

const ENDPOINT_FAMILY: GenericSignalFamily = GenericSignalFamily {
    generic_file: "otel_endpoint",
    generic_aliases: &[],
    generic_env: GENERIC_ENDPOINT,
    logs_file: "otel_logs_endpoint",
    logs_env: LOGS_ENDPOINT,
    metrics_file: "otel_metrics_endpoint",
    metrics_env: METRICS_ENDPOINT,
};

const PROTOCOL_FAMILY: GenericSignalFamily = GenericSignalFamily {
    generic_file: "otel_protocol",
    generic_aliases: &["otel_transport"],
    generic_env: GENERIC_PROTOCOL,
    logs_file: "otel_logs_protocol",
    logs_env: LOGS_PROTOCOL,
    metrics_file: "otel_metrics_protocol",
    metrics_env: METRICS_PROTOCOL,
};

const GENERIC_SIGNAL_FAMILIES: &[GenericSignalFamily] = &[ENDPOINT_FAMILY, PROTOCOL_FAMILY];

/// Client identity (cert/key): pin any member → strip unlisted siblings *and* unlisted endpoint family members.
/// CA is a sibling-only family: pin any CA member → strip unlisted CA env names. Does **not** strip endpoints (a fleet can pin a trust store without locking destination).
struct ClientIdentityFamily {
    members: &'static [(&'static str, &'static str)],
}

const CLIENT_CERT_FAMILY: ClientIdentityFamily = ClientIdentityFamily {
    members: &[
        (
            "otel_client_certificate",
            "OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE",
        ),
        (
            "otel_logs_client_certificate",
            "OTEL_EXPORTER_OTLP_LOGS_CLIENT_CERTIFICATE",
        ),
        (
            "otel_metrics_client_certificate",
            "OTEL_EXPORTER_OTLP_METRICS_CLIENT_CERTIFICATE",
        ),
    ],
};

const CLIENT_KEY_FAMILY: ClientIdentityFamily = ClientIdentityFamily {
    members: &[
        ("otel_client_key", "OTEL_EXPORTER_OTLP_CLIENT_KEY"),
        ("otel_logs_client_key", "OTEL_EXPORTER_OTLP_LOGS_CLIENT_KEY"),
        (
            "otel_metrics_client_key",
            "OTEL_EXPORTER_OTLP_METRICS_CLIENT_KEY",
        ),
    ],
};

const CLIENT_IDENTITY_FAMILIES: &[ClientIdentityFamily] = &[CLIENT_CERT_FAMILY, CLIENT_KEY_FAMILY];

const CA_CERT_FAMILY: ClientIdentityFamily = ClientIdentityFamily {
    members: &[
        ("otel_certificate", "OTEL_EXPORTER_OTLP_CERTIFICATE"),
        (
            "otel_logs_certificate",
            "OTEL_EXPORTER_OTLP_LOGS_CERTIFICATE",
        ),
        (
            "otel_metrics_certificate",
            "OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE",
        ),
    ],
};

impl GenericSignalFamily {
    fn generic_pinned(&self, listed: impl Fn(&str) -> bool) -> bool {
        listed(self.generic_file) || self.generic_aliases.iter().copied().any(listed)
    }

    fn apply(&self, strip: &mut HashSet<String>, listed: impl Fn(&str) -> bool) {
        if !self.generic_pinned(&listed) {
            return;
        }
        strip.insert(self.generic_env.into());
        if !listed(self.logs_file) {
            strip.insert(self.logs_env.into());
        }
        if !listed(self.metrics_file) {
            strip.insert(self.metrics_env.into());
        }
    }

    fn strip_unlisted_members(&self, strip: &mut HashSet<String>, listed: impl Fn(&str) -> bool) {
        if !self.generic_pinned(&listed) {
            strip.insert(self.generic_env.into());
        }
        if !listed(self.logs_file) {
            strip.insert(self.logs_env.into());
        }
        if !listed(self.metrics_file) {
            strip.insert(self.metrics_env.into());
        }
    }
}

impl ClientIdentityFamily {
    /// Strip unlisted siblings. Does not touch endpoints.
    fn apply_siblings(&self, strip: &mut HashSet<String>, listed: impl Fn(&str) -> bool) {
        if !self.members.iter().any(|(file, _)| listed(file)) {
            return;
        }
        for (file, env) in self.members {
            if !listed(file) {
                strip.insert((*env).into());
            }
        }
    }

    fn apply(&self, strip: &mut HashSet<String>, listed: impl Fn(&str) -> bool) {
        self.apply_siblings(strip, &listed);
        if self.members.iter().any(|(file, _)| listed(file)) {
            ENDPOINT_FAMILY.strip_unlisted_members(strip, listed);
        }
    }
}

/// Named table rule: listing any content gate traps omitted sibling gates
/// (force-off `"0"`) so developer env cannot inherit them. The same walk
/// produces the `"0"` overrides and the strip set.
fn apply_content_gate_inherit_trap(
    listed: impl Fn(&str) -> bool,
    mut overrides: Option<&mut HashMap<String, String>>,
    mut strip: Option<&mut HashSet<String>>,
) {
    if !CONTENT_GATE_KEYS.iter().copied().any(&listed) {
        return;
    }
    for key in CONTENT_GATE_KEYS {
        let Some(env) = otel_file_key_to_env(key) else {
            continue;
        };
        if let Some(strip) = strip.as_mut() {
            strip.insert(env.clone());
        }
        if !listed(key)
            && let Some(overrides) = overrides.as_mut()
        {
            overrides.entry(env).or_insert_with(|| "0".into());
        }
    }
}

/// Map a `[telemetry] otel_*` file key to the env var the resolver reads.
/// Mechanical: `otel_enabled` → `GROK_EXTERNAL_OTEL`; otherwise `OTEL_` + screaming-snake remainder — except the OTLP exporter family, whose spec names insert `EXPORTER_OTLP`. Headers are never mapped.
pub fn otel_file_key_to_env(key: &str) -> Option<String> {
    let rest = key.strip_prefix("otel_")?;
    if rest.contains("header") {
        return None;
    }
    Some(match rest {
        "enabled" => "GROK_EXTERNAL_OTEL".into(),
        "endpoint" => GENERIC_ENDPOINT.into(),
        "logs_endpoint" => LOGS_ENDPOINT.into(),
        "metrics_endpoint" => METRICS_ENDPOINT.into(),
        "protocol" | "transport" => GENERIC_PROTOCOL.into(),
        "logs_protocol" => LOGS_PROTOCOL.into(),
        "metrics_protocol" => METRICS_PROTOCOL.into(),
        "timeout" => "OTEL_EXPORTER_OTLP_TIMEOUT".into(),
        "certificate" => "OTEL_EXPORTER_OTLP_CERTIFICATE".into(),
        "logs_certificate" => "OTEL_EXPORTER_OTLP_LOGS_CERTIFICATE".into(),
        "metrics_certificate" => "OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE".into(),
        "client_certificate" => "OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE".into(),
        "logs_client_certificate" => "OTEL_EXPORTER_OTLP_LOGS_CLIENT_CERTIFICATE".into(),
        "metrics_client_certificate" => "OTEL_EXPORTER_OTLP_METRICS_CLIENT_CERTIFICATE".into(),
        "client_key" => "OTEL_EXPORTER_OTLP_CLIENT_KEY".into(),
        "logs_client_key" => "OTEL_EXPORTER_OTLP_LOGS_CLIENT_KEY".into(),
        "metrics_client_key" => "OTEL_EXPORTER_OTLP_METRICS_CLIENT_KEY".into(),
        other => format!("OTEL_{}", other.to_ascii_uppercase()),
    })
}

fn toml_to_env_value(v: &toml::Value) -> Option<String> {
    match v {
        toml::Value::Boolean(b) => Some(if *b { "1".into() } else { "0".into() }),
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

/// Listed `otel_*` pins plus inherit-trap defaults for omitted sibling gates.
#[derive(Debug, Default, Clone)]
pub struct RequirementOtelPins {
    /// Env name → pinned value (from requirements, or `"0"` for omitted gates).
    pub overrides: HashMap<String, String>,
    /// File keys that were actually present in requirements (not inherit-trap).
    pub listed_file_keys: HashSet<String>,
}

impl RequirementOtelPins {
    pub fn from_requirements(requirements: Option<&toml::Value>) -> Self {
        let Some(table) = requirements
            .and_then(|r| r.get("telemetry"))
            .and_then(|t| t.as_table())
        else {
            return Self::default();
        };

        let mut listed_file_keys = HashSet::new();
        let mut overrides = HashMap::new();
        for (key, value) in table {
            if !key.starts_with("otel_") {
                continue;
            }
            let Some(env) = otel_file_key_to_env(key) else {
                tracing::warn!(
                    key,
                    "external otel: ignoring requirements key (headers never live in TOML)"
                );
                continue;
            };
            let Some(rendered) = toml_to_env_value(value) else {
                continue;
            };
            listed_file_keys.insert(key.clone());
            overrides.insert(env, rendered);
        }

        apply_content_gate_inherit_trap(
            |k| listed_file_keys.contains(k),
            Some(&mut overrides),
            None,
        );

        Self {
            overrides,
            listed_file_keys,
        }
    }

    pub fn env_override(&self, name: &str) -> Option<String> {
        self.overrides.get(name).cloned()
    }

    fn listed(&self, file_key: &str) -> bool {
        self.listed_file_keys.contains(file_key)
    }

    /// Env names the production path must `remove_var` (pin strip matrix).
    pub fn names_to_strip(&self) -> Vec<String> {
        if self.listed_file_keys.is_empty() {
            return Vec::new();
        }
        let mut strip: HashSet<String> = HashSet::new();
        for key in &self.listed_file_keys {
            if let Some(env) = otel_file_key_to_env(key) {
                strip.insert(env);
            }
        }
        for family in GENERIC_SIGNAL_FAMILIES {
            family.apply(&mut strip, |k| self.listed(k));
        }
        for family in CLIENT_IDENTITY_FAMILIES {
            family.apply(&mut strip, |k| self.listed(k));
        }
        CA_CERT_FAMILY.apply_siblings(&mut strip, |k| self.listed(k));
        apply_content_gate_inherit_trap(|k| self.listed(k), None, Some(&mut strip));

        let mut out: Vec<String> = strip.into_iter().collect();
        out.sort();
        out
    }

    /// Drop unlisted `[telemetry] otel_*` file keys that the pin strip matrix would hide in env. Listed keys stay; keys outside the strip set stay (a timeout in user config still applies when only the endpoint is pinned).
    pub fn hide_unlisted_file_siblings(&self, table: &mut toml::map::Map<String, toml::Value>) {
        if self.listed_file_keys.is_empty() {
            return;
        }
        let strip: HashSet<String> = self.names_to_strip().into_iter().collect();
        table.retain(|key, _| {
            if !key.starts_with("otel_") || self.listed(key) {
                return true;
            }
            match otel_file_key_to_env(key) {
                Some(env) => !strip.contains(&env),
                None => true,
            }
        });
    }
}

/// Overlay requirements pins onto `getenv`. Does not mutate process env.
/// Unlisted names that the pin strip matrix would drop are hidden so `resolve_with` tests (injected getenv) match production after `remove_var`.
/// Pair with [`RequirementOtelPins::hide_unlisted_file_siblings`] so user and managed file config cannot fill those holes.
pub fn getenv_with_pins<'a>(
    pins: &'a RequirementOtelPins,
    getenv: impl Fn(&str) -> Option<String> + 'a,
) -> impl Fn(&str) -> Option<String> + 'a {
    let stripped: HashSet<String> = pins.names_to_strip().into_iter().collect();
    move |name: &str| {
        if let Some(v) = pins.env_override(name) {
            return Some(v);
        }
        if stripped.contains(name) {
            return None;
        }
        getenv(name)
    }
}

/// Strip conflicting developer `OTEL_*` from process env.
/// # Safety
/// `remove_var` is unsound beside concurrent `getenv`. Call once from pager-bin `main`, after clap/version/doctor, before `memory_trace::start`, Sentry, Tokio, `build_otel_layer`, or `external::init`. Not from `run()`, `init_tracing`, or `init_tracing_simple`.
pub unsafe fn strip_conflicting_process_env() {
    let Some(req) = distill_config::load_merged_requirements() else {
        return;
    };
    // SAFETY: caller contract — single-threaded composition root.
    let stripped = unsafe { apply_process_env_strip(&req) };
    if !stripped.is_empty() {
        tracing::debug!(
            keys = %stripped.join(", "),
            "external otel: stripped developer OTEL_* because requirements pinned …"
        );
    }
}

/// Production strip of conflicting developer OTEL_* names; returns stripped names.
/// # Safety
/// Same contract as [`strip_conflicting_process_env`]. Tests live in a dedicated binary so they cannot race the lib suite.
pub unsafe fn apply_process_env_strip(requirements: &toml::Value) -> Vec<String> {
    let pins = RequirementOtelPins::from_requirements(Some(requirements));
    let names = pins.names_to_strip();
    for name in &names {
        // SAFETY: caller contract — single-threaded, no concurrent getenv.
        unsafe { std::env::remove_var(name) };
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(toml: &str) -> toml::Value {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn mechanical_env_names() {
        assert_eq!(
            otel_file_key_to_env("otel_enabled").as_deref(),
            Some("GROK_EXTERNAL_OTEL")
        );
        assert_eq!(
            otel_file_key_to_env("otel_log_user_prompts").as_deref(),
            Some("OTEL_LOG_USER_PROMPTS")
        );
        assert_eq!(
            otel_file_key_to_env("otel_log_tool_content").as_deref(),
            Some("OTEL_LOG_TOOL_CONTENT")
        );
        assert_eq!(
            otel_file_key_to_env("otel_metrics_exporter").as_deref(),
            Some("OTEL_METRICS_EXPORTER")
        );
        assert_eq!(
            otel_file_key_to_env("otel_metric_export_interval").as_deref(),
            Some("OTEL_METRIC_EXPORT_INTERVAL")
        );
        assert_eq!(
            otel_file_key_to_env("otel_endpoint").as_deref(),
            Some(GENERIC_ENDPOINT)
        );
        assert_eq!(otel_file_key_to_env("otel_exporter_otlp_headers"), None);
    }

    #[test]
    fn inherit_trap_omitted_assistant_defaults_off() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_log_user_prompts = true
            "#)));
        assert_eq!(
            pins.env_override("OTEL_LOG_USER_PROMPTS").as_deref(),
            Some("1")
        );
        assert_eq!(
            pins.env_override("OTEL_LOG_ASSISTANT_RESPONSES").as_deref(),
            Some("0")
        );
        assert_eq!(
            pins.env_override("OTEL_LOG_TOOL_DETAILS").as_deref(),
            Some("0")
        );
        assert_eq!(
            pins.env_override("OTEL_LOG_TOOL_CONTENT").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn env_only_keeps_assistant_unset_when_no_requirements_gates() {
        let pins = RequirementOtelPins::from_requirements(None);
        assert!(pins.env_override("OTEL_LOG_ASSISTANT_RESPONSES").is_none());
    }

    #[test]
    fn content_gate_inherit_trap_strips_omitted_sibling_env() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_log_user_prompts = true
            "#)));
        let names = pins.names_to_strip();
        assert!(names.contains(&"OTEL_LOG_USER_PROMPTS".to_string()));
        assert!(names.contains(&"OTEL_LOG_ASSISTANT_RESPONSES".to_string()));
        assert!(names.contains(&"OTEL_LOG_TOOL_DETAILS".to_string()));
        assert!(names.contains(&"OTEL_LOG_TOOL_CONTENT".to_string()));
    }

    #[test]
    fn pin_endpoint_hides_unlisted_per_signal_env() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_endpoint = "http://corp:4318"
            "#)));
        let names = pins.names_to_strip();
        assert!(names.contains(&GENERIC_ENDPOINT.into()));
        assert!(names.contains(&LOGS_ENDPOINT.into()));
        assert!(names.contains(&METRICS_ENDPOINT.into()));
        let getenv = getenv_with_pins(&pins, |name| match name {
            LOGS_ENDPOINT => Some("http://127.0.0.1:9".into()),
            _ => None,
        });
        assert_eq!(
            getenv(GENERIC_ENDPOINT).as_deref(),
            Some("http://corp:4318")
        );
        assert_eq!(
            getenv(LOGS_ENDPOINT),
            None,
            "unlisted per-signal decoy hidden"
        );
    }

    #[test]
    fn pin_endpoint_hides_unlisted_file_siblings() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_endpoint = "http://corp:4318"
            "#)));
        let mut table = req(r#"
            otel_endpoint = "http://user:4318"
            otel_logs_endpoint = "http://127.0.0.1:9/v1/logs"
            otel_timeout = 5000
            "#)
        .as_table()
        .cloned()
        .unwrap();
        pins.hide_unlisted_file_siblings(&mut table);
        assert!(
            table.contains_key("otel_timeout"),
            "unrelated file key stays"
        );
        assert!(
            !table.contains_key("otel_logs_endpoint"),
            "unlisted per-signal file sibling must not retarget"
        );
        assert!(
            table.contains_key("otel_endpoint"),
            "generic file key is not a sibling of itself"
        );
    }

    #[test]
    fn pin_endpoint_keeps_listed_per_signal_file_key() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_endpoint = "http://corp:4318"
            otel_logs_endpoint = "http://logs:4318/v1/logs"
            "#)));
        let mut table = req(r#"
            otel_logs_endpoint = "http://127.0.0.1:9/v1/logs"
            otel_metrics_endpoint = "http://127.0.0.1:9/v1/metrics"
            "#)
        .as_table()
        .cloned()
        .unwrap();
        pins.hide_unlisted_file_siblings(&mut table);
        assert!(table.contains_key("otel_logs_endpoint"));
        assert!(!table.contains_key("otel_metrics_endpoint"));
    }

    #[test]
    fn pin_endpoint_keeps_listed_per_signal_value() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_endpoint = "http://corp:4318"
            otel_logs_endpoint = "http://logs:4318/v1/logs"
            "#)));
        let getenv = getenv_with_pins(&pins, |name| match name {
            LOGS_ENDPOINT => Some("http://127.0.0.1:9".into()),
            METRICS_ENDPOINT => Some("http://127.0.0.1:9".into()),
            _ => None,
        });
        assert_eq!(
            getenv(LOGS_ENDPOINT).as_deref(),
            Some("http://logs:4318/v1/logs")
        );
        assert_eq!(getenv(METRICS_ENDPOINT), None);
    }

    #[test]
    fn pin_ca_does_not_strip_endpoints() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_certificate = "/etc/ssl/corp-ca.pem"
            "#)));
        let names = pins.names_to_strip();
        assert!(!names.iter().any(|n| ENDPOINT_ENV.contains(&n.as_str())));
        assert!(names.contains(&"OTEL_EXPORTER_OTLP_CERTIFICATE".to_string()));
        assert!(
            names.contains(&"OTEL_EXPORTER_OTLP_LOGS_CERTIFICATE".to_string()),
            "generic CA pin must hide unlisted per-signal CA env"
        );
        assert!(names.contains(&"OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE".to_string()));
    }

    #[test]
    fn pin_ca_strips_unlisted_sibling_certs() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_logs_certificate = "/etc/ssl/logs-ca.pem"
            "#)));
        let names = pins.names_to_strip();
        assert!(names.contains(&"OTEL_EXPORTER_OTLP_CERTIFICATE".to_string()));
        assert!(names.contains(&"OTEL_EXPORTER_OTLP_LOGS_CERTIFICATE".to_string()));
        assert!(names.contains(&"OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE".to_string()));
        assert!(
            !names.iter().any(|n| ENDPOINT_ENV.contains(&n.as_str())),
            "CA family must not strip endpoints"
        );
        let getenv = getenv_with_pins(&pins, |name| match name {
            "OTEL_EXPORTER_OTLP_CERTIFICATE" => Some("/tmp/decoy-ca.pem".into()),
            "OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE" => Some("/tmp/decoy-metrics-ca.pem".into()),
            _ => None,
        });
        assert_eq!(
            getenv("OTEL_EXPORTER_OTLP_LOGS_CERTIFICATE").as_deref(),
            Some("/etc/ssl/logs-ca.pem")
        );
        assert_eq!(getenv("OTEL_EXPORTER_OTLP_CERTIFICATE"), None);
        assert_eq!(getenv("OTEL_EXPORTER_OTLP_METRICS_CERTIFICATE"), None);
    }

    #[test]
    fn pin_client_cert_strips_endpoints() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_client_certificate = "/etc/ssl/client.crt"
            otel_client_key = "/etc/ssl/client.key"
            "#)));
        let names = pins.names_to_strip();
        for n in ENDPOINT_ENV {
            assert!(names.contains(&n.to_string()), "must strip {n}");
        }
    }

    #[test]
    fn pin_protocol_strips_per_signal_protocol() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_protocol = "http/protobuf"
            "#)));
        let names = pins.names_to_strip();
        for n in PROTOCOL_ENV {
            assert!(names.contains(&n.to_string()), "must strip {n}");
        }
    }

    #[test]
    fn exporters_lock_only_when_listed() {
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_endpoint = "http://corp:4318"
            "#)));
        assert!(pins.env_override("OTEL_LOGS_EXPORTER").is_none());
        let pins = RequirementOtelPins::from_requirements(Some(&req(r#"
            [telemetry]
            otel_logs_exporter = "otlp"
            "#)));
        assert_eq!(
            pins.env_override("OTEL_LOGS_EXPORTER").as_deref(),
            Some("otlp")
        );
    }
}
