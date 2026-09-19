// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;
use std::time::Duration;
use distill_auth::AuthCredentialProvider;
pub(crate) const MAX_EXPORT_BATCH_SIZE: usize = 64;
pub(crate) const MAX_QUEUE_SIZE: usize = 8192;
pub(crate) const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
pub struct OtelLayerConfig {
    pub credentials: Arc<dyn AuthCredentialProvider>,
    pub token_header_value: String,
    pub alpha_test_key: Option<String>,
    pub exporter: OtelExporterConfig,
}
#[derive(Debug, Clone, Copy)]
pub struct OtelClientInfo {
    pub client_name: &'static str,
    pub client_version: &'static str,
    pub service_version: &'static str,
    pub app_entrypoint: &'static str,
}
#[derive(Debug, Default, Clone)]
pub struct OtelExporterConfig {
    pub traces_url: String,
    pub extra_headers: Vec<(String, String)>,
    pub export_interval: Option<Duration>,
    pub timeout: Option<Duration>,
    pub enabled: bool,
}
pub(crate) fn build_base_resource(client: OtelClientInfo) -> opentelemetry_sdk::Resource {
    let OtelClientInfo {
        client_name,
        client_version,
        service_version,
        app_entrypoint,
    } = client;
    let mut resource_attrs = vec![
        opentelemetry::KeyValue::new("service.version", service_version.to_string()),
        opentelemetry::KeyValue::new("client.name", client_name.to_string()),
        opentelemetry::KeyValue::new("client.version", client_version.to_string()),
        opentelemetry::KeyValue::new("app.entrypoint", app_entrypoint.to_string()),
    ];
    if let Some(terminal_type) = std::env::var("TERM_PROGRAM")
        .ok()
        .or_else(|| std::env::var("TERM").ok())
        .filter(|v| !v.is_empty())
    {
        resource_attrs.push(opentelemetry::KeyValue::new("terminal.type", terminal_type));
    }
    opentelemetry_sdk::Resource::builder_empty()
        .with_service_name("grok-cli")
        .with_attributes(resource_attrs)
        .build()
}
pub(crate) fn build_static_headers(
    client_version: &str,
    alpha_test_key: Option<String>,
    traces_url: &str,
) -> std::collections::HashMap<String, String> {
    let mut static_headers = std::collections::HashMap::new();
    static_headers.insert(
        "x-grok-client-version".to_string(),
        client_version.to_string(),
    );
    let _ = (alpha_test_key, traces_url);
    static_headers
}
pub(crate) fn build_export_headers(
    static_headers: &std::collections::HashMap<String, String>,
    token: &str,
    token_auth_header: Option<&str>,
    extra_headers: &[(String, String)],
    snapshot: &distill_auth::CredentialSnapshot,
) -> std::collections::HashMap<String, String> {
    let mut headers = static_headers.clone();
    for (name, value) in [
        ("x-userid", &snapshot.user_id),
        ("x-teamid", &snapshot.team_id),
    ] {
        match value.as_deref().filter(|v| !v.is_empty()) {
            Some(v) => {
                headers.insert(name.to_string(), v.to_string());
            }
            None => {
                headers.remove(name);
            }
        }
    }
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    if let Some(value) = token_auth_header {
        headers.insert("X-XAI-Token-Auth".to_string(), value.to_string());
    }
    for (k, v) in extra_headers {
        headers.insert(k.clone(), v.clone());
    }
    headers
}
pub(crate) fn resource_with_tenant_id(
    base: opentelemetry_sdk::Resource,
    snapshot: &distill_auth::CredentialSnapshot,
) -> opentelemetry_sdk::Resource {
    let tenant_attrs: Vec<opentelemetry::KeyValue> = [
        ("deployment.id", &snapshot.deployment_id),
        ("api_key.id", &snapshot.api_key_id),
        ("organization.id", &snapshot.organization_id),
        ("team.id", &snapshot.team_id),
        ("user.id", &snapshot.user_id),
    ]
    .into_iter()
    .filter_map(|(key, val)| {
        val.as_deref()
            .filter(|v| !v.is_empty())
            .map(|v| opentelemetry::KeyValue::new(key, v.to_string()))
    })
    .collect();
    if tenant_attrs.is_empty() {
        return base;
    }
    let mut attrs: Vec<opentelemetry::KeyValue> = base
        .iter()
        .map(|(k, v)| opentelemetry::KeyValue::new(k.clone(), v.clone()))
        .collect();
    attrs.extend(tenant_attrs);
    opentelemetry_sdk::Resource::builder_empty()
        .with_attributes(attrs)
        .build()
}
#[cfg(test)]
mod tests {
    use super::*;
    use distill_auth::CredentialSnapshot;
    #[test]
    fn build_export_headers_tracks_snapshot_and_respects_overrides() {
        let static_headers = std::collections::HashMap::new();
        for snapshot in [
            CredentialSnapshot::default(),
            CredentialSnapshot {
                user_id: Some(String::new()),
                team_id: Some(String::new()),
                ..Default::default()
            },
        ] {
            let headers = build_export_headers(&static_headers, "tok", None, &[], &snapshot);
            assert!(!headers.contains_key("x-userid"));
            assert!(!headers.contains_key("x-teamid"));
        }
        let snapshot = CredentialSnapshot {
            user_id: Some("u1".into()),
            team_id: Some("t9".into()),
            ..Default::default()
        };
        let extra = vec![("Authorization".to_string(), "Bearer custom".to_string())];
        let headers = build_export_headers(&static_headers, "auto-token", None, &extra, &snapshot);
        assert_eq!(headers.get("x-userid").map(String::as_str), Some("u1"));
        assert_eq!(headers.get("x-teamid").map(String::as_str), Some("t9"));
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer custom")
        );
    }
    #[test]
    fn resource_injects_tenant_id_attrs() {
        use opentelemetry::Key;
        let base = opentelemetry_sdk::Resource::builder_empty()
            .with_attributes([opentelemetry::KeyValue::new("user.id", "")])
            .build();
        let plain = resource_with_tenant_id(base.clone(), &CredentialSnapshot::default());
        assert!(plain.get(&Key::from("deployment.id")).is_none());
        assert!(plain.get(&Key::from("api_key.id")).is_none());
        let snap = CredentialSnapshot {
            deployment_id: Some("dep-7b97".into()),
            ..Default::default()
        };
        let r = resource_with_tenant_id(base.clone(), &snap);
        assert_eq!(
            r.get(&Key::from("deployment.id")).map(|v| v.to_string()),
            Some("dep-7b97".to_string())
        );
        assert!(
            r.get(&Key::from("user.id")).is_some(),
            "base attrs preserved"
        );
        let snap = CredentialSnapshot {
            api_key_id: Some("ak-0c2b".into()),
            ..Default::default()
        };
        let r = resource_with_tenant_id(base.clone(), &snap);
        assert_eq!(
            r.get(&Key::from("api_key.id")).map(|v| v.to_string()),
            Some("ak-0c2b".to_string())
        );
        let snap = CredentialSnapshot {
            organization_id: Some("org-abc".into()),
            ..Default::default()
        };
        let r = resource_with_tenant_id(base.clone(), &snap);
        assert_eq!(
            r.get(&Key::from("organization.id")).map(|v| v.to_string()),
            Some("org-abc".to_string())
        );
        let snap = CredentialSnapshot {
            user_id: Some("user-42".into()),
            ..Default::default()
        };
        let r = resource_with_tenant_id(base.clone(), &snap);
        assert_eq!(
            r.get(&Key::from("user.id")).map(|v| v.to_string()),
            Some("user-42".to_string())
        );
        let snap = CredentialSnapshot {
            deployment_id: Some("dep-9".into()),
            user_id: Some(String::new()),
            organization_id: Some(String::new()),
            team_id: Some(String::new()),
            ..Default::default()
        };
        let r =
            resource_with_tenant_id(opentelemetry_sdk::Resource::builder_empty().build(), &snap);
        assert!(r.get(&Key::from("user.id")).is_none());
        assert!(r.get(&Key::from("organization.id")).is_none());
        assert!(r.get(&Key::from("team.id")).is_none());
        assert_eq!(
            r.get(&Key::from("deployment.id")).map(|v| v.to_string()),
            Some("dep-9".to_string())
        );
    }
}
